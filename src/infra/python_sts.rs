//! 转发 python 后端签发 STS 临时凭证
//!
//! 2026-09-18 新增。背景：
//! - 原 backend-rust `infra::sts::TencentSts` 直连腾讯云 `sts.tencentcloudapi.com`，
//!   绕过 python 后端的凭据管理 + 权限审计 + 限流。
//! - 新设计：rust 后端通过 HTTP 转发到 python 后端内部端点
//!   `POST {PYTHON_BACKEND_BASE_URL}/api/v1/files/sts-prefix-credentials`，
//!   body `{prefix, expire_seconds}`；让 python 端统一管 STS 凭据 / 审计 / 限流，
//!   rust 仅做"传话筒"。
//!
//! ## 设计要点
//! - **trait `PythonSts`** + `Arc<dyn>` 注入；Noop 占位供本地 `cargo run` 不依赖
//!   python 后端跑通（与 `infra::cos::NoopCos` / `NoopSts` 同形）。
//! - 超时 10s（reqwest 默认无超时；长任务需显式）。HTTP 4xx/5xx 映射到
//!   `BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED` (21608)。
//! - 不重试：转发失败由调用方决定（业务上让前端走完整重试链路更直观）。
//!
//! ## 鉴权策略（2026-09-18 review #6 明确）
//! - 当前**不携带任何鉴权 header**，依赖 docker 内网隔离（rust ↔ python 走 backend
//!   service 之间的 compose network；该端点目前**裸开**无 token / mTLS）。
//! - 若未来 python 端加 token 鉴权（如 `Authorization: Bearer <internal-token>`），
//!   在此 `.send().await` 前显式构造 header，并从环境变量读取 token：
//!   ```ignore
//!   let token = env::var("PYTHON_BACKEND_INTERNAL_TOKEN").unwrap_or_default();
//!   req = req.bearer_auth(token);
//!   ```
//! - 不要把 token 硬编码进代码或写入 git。
//!
//! ## 响应体格式（python 后端契约，2026-09-18 锁定）
//!
//! python 后端所有响应过 `core/middleware.py::UnifiedResponseMiddleware` 包装成
//! **统一信封**：
//!
//! ```jsonc
//! {
//!   "code": 0,                    // 业务码：0 成功；非 0 业务异常
//!   "message": "ok",              // 人读消息
//!   "data": { /* 实际响应体 */ } // 业务数据（成功时存在；业务异常时常为 null）
//! }
//! ```
//!
//! **成功**（HTTP 2xx）：`code == 0`、`data` 是 STS 凭证对象：
//!
//! ```jsonc
//! {
//!   "code": 0,
//!   "message": "ok",
//!   "data": {
//!     "tmp_secret_id": "...",
//!     "tmp_secret_key": "...",
//!     "session_token": "...",
//!     "start_time": 1734567890,    // unix 秒
//!     "expired_time": 1734571490,  // unix 秒
//!     "bucket": "...",
//!     "region": "...",
//!     "tmp_prefix": "tmp/sess/<uuid>/"
//!   }
//! }
//! ```
//!
//! **业务异常**（HTTP 4xx/5xx）：`code != 0`、`data == null`，例如：
//! ```jsonc
//! {
//!   "code": 21503,
//!   "message": "权限不足",
//!   "data": null
//! }
//! ```
//!
//! **rust 端契约**：
//! - 解析顺序：先反序列化为 `PythonEnvelope<T>` 解信封；再看 `code` / `data`。
//! - 成功路径：直接返回 `data`（= `PythonStsCredential`）。
//! - 业务异常路径：把 python 的 `code` / `message` **透传**到 rust 错误消息里
//!   （`"python STS 业务错误 [{code}] {message}"`），前端能看到原始错码（如 21503）。
//! - 解析失败（malformed JSON / data 字段缺失）：映射到 `BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED` (21608)，
//!   错误消息带 `status` + body 前 200 字节预览便于排查。
//!
//! 解析逻辑抽到纯函数 [`parse_python_response`]（module-private，单测友好，
//! 不依赖 HTTP）。2026-09-18 修复 21608 时加入。

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::shared::error::{AppError, code};

/// 单次签发的 STS 凭证（python 端响应 → rust 内）。
///
/// 字段命名贴近 python JSON：i64 时间戳直接走默认反序列化（前端拿到时由 DTO 层
/// `serialize_i64` 决定是否转字符串）。
///
/// 2026-09-18 新增。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonStsCredential {
    pub tmp_secret_id: String,
    pub tmp_secret_key: String,
    pub session_token: String,
    #[serde(default)]
    pub start_time: i64,
    pub expired_time: i64,
    /// COS bucket（python 端负责从配置读，rust 不再持 bucket 配置）。
    pub bucket: String,
    /// COS region（同上）。
    pub region: String,
    /// 写入 prefix（含尾斜杠）。python 端按 `tmp/sess/<uuid>/` 派生后回传。
    pub tmp_prefix: String,
}

/// 凭证签发 trait（main.rs 装配时按 `PYTHON_BACKEND_BASE_URL` 是否可达 / 是否开启二选一）。
///
/// 2026-09-18 新增。
#[async_trait]
pub trait PythonSts: Send + Sync {
    /// 调 python 后端签发 prefix-scoped STS 凭证。
    ///
    /// `prefix`：调用方拼好的 tmp 前缀（含尾斜杠；rust 这边不二次拼）。
    /// `expire_seconds`：期望有效期（python 端可能按配置上下限收敛）。
    async fn issue(
        &self,
        prefix: &str,
        expire_seconds: u32,
    ) -> Result<PythonStsCredential, AppError>;
}

/// python 后端统一响应信封（`UnifiedResponseMiddleware` 包装）。
///
/// 2026-09-18 新增。rust 端必须**先**反序列化为此层，再判断 `code` / 取 `data`——
/// 直接反序列化为业务 DTO 会因字段对不上而失败（21608 历史 bug 根因）。
#[derive(Debug, Deserialize)]
struct PythonEnvelope<T> {
    /// python 业务码。0 成功；非 0 业务异常（与 rust `AppError::code()` 同号位空间）。
    code: i32,
    /// 人读消息。python 业务异常时携带具体原因（如"权限不足"）。
    /// 反序列化允许为空（python middleware 在系统异常场景可能给空串），
    /// `#[serde(default)]` 容错。
    #[serde(default)]
    message: String,
    /// 实际响应体。成功时是 `T`；业务异常时通常是 `null`。
    /// `Option<T>` 是为了"code==0 但 data 缺字段 / 为 null"时反序列化成功
    /// （让上层做语义校验，而不是直接 serde 失败）。
    data: Option<T>,
}

/// 从 python 响应 body 解信封 + 取业务数据。
///
/// 2026-09-18 新增。**纯函数**——只依赖入参，不访问网络 / 全局状态；
/// 单测覆盖所有分支，无需 mock。
///
/// ## 判定规则
///
/// 1. 反序列化为 [`PythonEnvelope<PythonStsCredential>`]：
///    - **成功 + `code == 0` + `data == Some(cred)`** → `Ok(cred)`
///    - **成功 + `code == 0` + `data == None`** → `Err(BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED)`，
///      消息：`"python STS 响应成功但 data 为空: code=0, message=<原 message>"`
///    - **成功 + `code != 0`**（python 业务异常）→ `Err(BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED)`，
///      消息：`"python STS 业务错误 [{code}] {message}"`（python 错码 + 原始 message
///      透传给前端，便于排查）——`status` 入参**不**进消息（业务异常时 HTTP 状态码
///      不是 4xx/5xx 信噪比低于业务码）
///    - **反序列化失败**（malformed JSON / `data` 字段类型不匹配）→
///      `Err(BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED)`，消息：`"python STS 响应解析失败:
///      status={status}, error={serde_err}, body_preview=<前 200 字节 utf8 lossless>"`
///
/// ## 入参
///
/// - `status`：HTTP 状态码（`reqwest::Response::status()`）。仅在"反序列化失败"
///   分支进错误消息，便于排查"python 端突然返回 HTML 错误页"等情况。
/// - `body`：HTTP 响应原始 bytes（即使是错误状态码也要把 body 读完传入，便于诊断）。
///
/// ## 与 HTTP 层解耦
///
/// 本函数不读 `reqwest::Response`——调用方在 `HttpPythonSts::issue` 里先
/// `resp.bytes().await` 取 bytes，再调本函数。HTTP 错误（reqwest 内部错误 / 超时）
/// 由调用方独立映射到 `BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED`，不进本函数。
fn parse_python_response(status: StatusCode, body: &[u8]) -> Result<PythonStsCredential, AppError> {
    let envelope: Result<PythonEnvelope<PythonStsCredential>, _> = serde_json::from_slice(body);

    match envelope {
        // 1) 信封解析成功
        Ok(env) => {
            if env.code == 0 {
                match env.data {
                    Some(cred) => Ok(cred),
                    None => Err(AppError::biz(
                        code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED,
                        format!(
                            "python STS 响应成功但 data 为空: code={}, message={}",
                            env.code, env.message
                        ),
                    )),
                }
            } else {
                // 业务异常：python 的 code + message 透传到 rust 错误消息
                Err(AppError::biz(
                    code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED,
                    format!("python STS 业务错误 [{}] {}", env.code, env.message),
                ))
            }
        }
        // 2) 信封解析失败（含 JSON 非法 + data 字段类型/缺字段）
        Err(e) => {
            // body 前 200 字节预览（lossless UTF-8 转换便于日志/前端展示）
            let preview = String::from_utf8_lossy(&body[..body.len().min(200)]);
            Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED,
                format!(
                    "python STS 响应解析失败: status={}, error={}, body_preview={}",
                    status, e, preview
                ),
            ))
        }
    }
}

// ============================================================
// HttpPythonSts：真实实现
// ============================================================

/// HTTP 实现：调 `{PYTHON_BACKEND_BASE_URL}/api/v1/files/sts-prefix-credentials`。
///
/// 2026-09-18 新增。
pub struct HttpPythonSts {
    base_url: String,
    client: Client,
}

impl HttpPythonSts {
    pub fn new(base_url: String) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow::anyhow!("reqwest client build 失败: {e}"))?;
        Ok(Self { base_url, client })
    }
}

/// python 端请求 body（最小契约；prefix + expire_seconds）。
#[derive(Debug, Serialize)]
struct IssueReq<'a> {
    prefix: &'a str,
    expire_seconds: u32,
}

#[async_trait]
impl PythonSts for HttpPythonSts {
    async fn issue(
        &self,
        prefix: &str,
        expire_seconds: u32,
    ) -> Result<PythonStsCredential, AppError> {
        let url = format!("{}/api/v1/files/sts-prefix-credentials", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&IssueReq {
                prefix,
                expire_seconds,
            })
            .send()
            .await
            .map_err(|e| {
                warn!(prefix = %prefix, error = %e, "转发 python STS HTTP 请求失败");
                AppError::biz(
                    code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED,
                    format!("转发 python STS 失败: {e}"),
                )
            })?;

        // 即使 status 是 4xx/5xx 也要读 body（python 端的统一信封可能在错误时
        // 仍有可解析结构，便于诊断；malformed 也走 parse_python_response 兜底）
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| {
            warn!(prefix = %prefix, error = %e, "读 python STS 响应 body 失败");
            AppError::biz(
                code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED,
                format!("读 python STS 响应 body 失败: {e}"),
            )
        })?;

        parse_python_response(status, &body)
    }
}

// ============================================================
// NoopPythonSts：本地占位
// ============================================================

/// 占位实现：本地 `cargo run` 不依赖 python 后端时返回合法占位凭证。
///
/// 2026-09-18 新增。
pub struct NoopPythonSts;

#[async_trait]
impl PythonSts for NoopPythonSts {
    async fn issue(
        &self,
        prefix: &str,
        _expire_seconds: u32,
    ) -> Result<PythonStsCredential, AppError> {
        // 占位实现给个 1h 过期；prefix 直接回传（caller 已规范化）。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(PythonStsCredential {
            tmp_secret_id: "AKIDnoop".to_string(),
            tmp_secret_key: "noop".to_string(),
            session_token: "noop-token".to_string(),
            start_time: now,
            expired_time: now + 3600,
            bucket: "noop-bucket".to_string(),
            region: "ap-shanghai".to_string(),
            tmp_prefix: prefix.to_string(),
        })
    }
}

// ============================================================
// 单测
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // parse_python_response 单测（覆盖所有分支）
    // 2026-09-18 新增。纯函数，不依赖 HTTP——mockito 不必要。
    // ============================================================

    /// 拼一个完整的 python 信封 JSON 字符串（含 data）。
    /// `code=0` + 完整 `data` → 解析成功 + `code==0` + `Some(cred)` 三条件全满足。
    #[test]
    fn parse_python_success_envelope() {
        let body = serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": {
                "tmp_secret_id": "AKIDxxx",
                "tmp_secret_key": "skxxx",
                "session_token": "tokxxx",
                "start_time": 1_700_000_000_i64,
                "expired_time": 1_700_003_600_i64,
                "bucket": "my-bucket-123",
                "region": "ap-shanghai",
                "tmp_prefix": "tmp/sess/abc/",
            }
        })
        .to_string();
        let cred =
            parse_python_response(StatusCode::OK, body.as_bytes()).expect("完整信封应解析成功");
        assert_eq!(cred.tmp_secret_id, "AKIDxxx");
        assert_eq!(cred.tmp_secret_key, "skxxx");
        assert_eq!(cred.session_token, "tokxxx");
        assert_eq!(cred.start_time, 1_700_000_000);
        assert_eq!(cred.expired_time, 1_700_003_600);
        assert_eq!(cred.bucket, "my-bucket-123");
        assert_eq!(cred.region, "ap-shanghai");
        assert_eq!(cred.tmp_prefix, "tmp/sess/abc/");
    }

    /// `code=0` + `data=null`（python 中间件成功但 payload 缺失）→ Err 且消息明确
    /// "data 为空"，原 `message` 也进错误消息便于诊断。
    #[test]
    fn parse_python_success_envelope_data_none() {
        let body = br#"{"code":0,"message":"ok","data":null}"#;
        let err = parse_python_response(StatusCode::OK, body).expect_err("data 缺失应失败");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
        let msg = err.to_string();
        assert!(msg.contains("data 为空"), "错误消息应指出 data 为空: {msg}");
        assert!(
            msg.contains("code=0") && msg.contains("message=ok"),
            "错误消息应含原信封字段: {msg}"
        );
    }

    /// python 业务异常 + HTTP 200（python 端偶有"业务错但 HTTP 仍 2xx"场景）→
    /// Err 且消息透传 `[{21503}] 权限不足`。
    #[test]
    fn parse_python_error_envelope_2xx() {
        // 中文 message 用普通字符串字面量 + as_bytes（raw byte literal 不允许非 ASCII）
        let body = "{\"code\":21503,\"message\":\"权限不足\",\"data\":null}".as_bytes();
        let err = parse_python_response(StatusCode::OK, body).expect_err("业务异常应失败");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
        let msg = err.to_string();
        assert!(
            msg.contains("[21503]") && msg.contains("权限不足"),
            "错误消息应透传 python code + message: {msg}"
        );
    }

    /// python 业务异常 + HTTP 400 → Err 且消息含 `[{21503}]`；
    /// HTTP status 不进消息（业务码信息量更大，避免双计数干扰）。
    #[test]
    fn parse_python_error_envelope_4xx() {
        let body = br#"{"code":21503,"message":"token expired","data":null}"#;
        let err = parse_python_response(StatusCode::BAD_REQUEST, body).expect_err("业务异常应失败");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
        let msg = err.to_string();
        assert!(
            msg.contains("[21503]") && msg.contains("token expired"),
            "错误消息应透传 python code + message: {msg}"
        );
    }

    /// HTTP 200 + 非 JSON body（python 中间件配错或 nginx 502 拦截）→ Err，
    /// 消息含"响应解析失败" + status + body 前 200 字节预览。
    #[test]
    fn parse_python_malformed_json_2xx() {
        let body = b"<html>502 Bad Gateway</html>";
        let err = parse_python_response(StatusCode::OK, body).expect_err("malformed JSON 应失败");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
        let msg = err.to_string();
        assert!(
            msg.contains("响应解析失败"),
            "错误消息应指出解析失败: {msg}"
        );
        assert!(
            msg.contains("status=200") && msg.contains("<html>502 Bad Gateway"),
            "错误消息应含 status + body 预览: {msg}"
        );
    }

    /// 空 body（python 端崩溃 / 中间件提前断流）→ Err（malformed 走解析失败分支）。
    #[test]
    fn parse_python_empty_body() {
        let err = parse_python_response(StatusCode::INTERNAL_SERVER_ERROR, b"")
            .expect_err("空 body 应失败");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
        assert!(
            err.to_string().contains("响应解析失败"),
            "空 body 应走解析失败分支"
        );
    }

    /// `code=0` 但 `data` 缺字段（python 端契约漂移 / 字段漏传）→ Err
    /// （serde 缺字段 → `PythonEnvelope.data: Option<T>` 反序列化仍成功但 inner 失败
    /// → 这里实际是 outer 失败）。消息含 body 预览便于排查。
    #[test]
    fn parse_python_wrong_shape_data() {
        // 缺 tmp_secret_key / bucket / region / tmp_prefix
        let body = br#"{"code":0,"message":"ok","data":{"tmp_secret_id":"x","session_token":"t","start_time":1,"expired_time":2}}"#;
        let err = parse_python_response(StatusCode::OK, body).expect_err("data 缺字段应失败");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
        let msg = err.to_string();
        assert!(
            msg.contains("响应解析失败") && msg.contains("body_preview="),
            "错误消息应含解析失败标识 + body 预览: {msg}"
        );
    }

    /// body 超 200 字节 → preview 仅截前 200 字节（防日志爆炸）。
    #[test]
    fn parse_python_long_body_preview_truncated_to_200_bytes() {
        let mut body = b"<html>".to_vec();
        body.extend(std::iter::repeat_n(b'x', 500));
        body.extend(b"</html>");
        let err =
            parse_python_response(StatusCode::BAD_GATEWAY, &body).expect_err("malformed 应失败");
        let msg = err.to_string();
        // preview 长度上限 200；body = "<html>"(6) + 500 x + "</html>"(7)，
        // preview = "<html>" + 194 个 x（共 200 字节），不含 "</html>"。
        assert!(
            msg.contains(&"x".repeat(194)),
            "preview 应包含截断后的 194 个 x: {msg}"
        );
        assert!(
            !msg.contains("</html>"),
            "preview 不应包含 200 字节之后的 </html>: {msg}"
        );
    }

    // ============================================================
    // 既有单测
    // ============================================================

    #[tokio::test]
    async fn noop_python_sts_returns_valid_placeholder() {
        let s = NoopPythonSts;
        let cred = s.issue("tmp/sess/abc/", 3600).await.expect("Noop 不抛错");
        assert_eq!(cred.tmp_prefix, "tmp/sess/abc/");
        assert!(cred.expired_time > cred.start_time);
        assert_eq!(cred.bucket, "noop-bucket");
        assert_eq!(cred.region, "ap-shanghai");
    }

    #[test]
    fn python_sts_credential_roundtrips_json() {
        let c = PythonStsCredential {
            tmp_secret_id: "id".into(),
            tmp_secret_key: "key".into(),
            session_token: "tok".into(),
            start_time: 100,
            expired_time: 3700,
            bucket: "b".into(),
            region: "r".into(),
            tmp_prefix: "tmp/sess/abc/".into(),
        };
        let json = serde_json::to_string(&c).expect("serialize");
        let back: PythonStsCredential = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.tmp_secret_id, "id");
        assert_eq!(back.expired_time, 3700);
    }
}
