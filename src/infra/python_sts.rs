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
//! **成功**（HTTP 2xx）：`code == 0`、`data` 是 [`StsPrefixCredentialsResponse`]
//! （`backend-python/schema/sts.py:118-127`）— 嵌套结构，凭证块在 `credentials`
//! 字段下：
//!
//! ```jsonc
//! {
//!   "code": 0,
//!   "message": "ok",
//!   "data": {
//!     "credentials": {
//!       "tmp_secret_id": "...",
//!       "tmp_secret_key": "...",
//!       "session_token": "...",
//!       "start_time": 1734567890,    // unix 秒
//!       "expired_time": 1734571490   // unix 秒
//!     },
//!     "start_time": 1734567890,      // unix 秒（与 credentials.start_time 同步）
//!     "expired_time": 1734571490,    // unix 秒（与 credentials.expired_time 同步）
//!     "expires_in": 3600,
//!     "bucket": "...",
//!     "region": "...",
//!     "endpoint": "https://cos.ap-shanghai.myqcloud.com",
//!     "scheme": "https"
//!   }
//! }
//! ```
//!
//! > 注：python 端 schema 不返回 `tmp_prefix`——rust 调用方在 `issue(prefix, ...)`
//! > 时已收 prefix 入参（`tmp/sess/<uuid>/`），由 rust 端自行持有，**不**回读。
//! > `endpoint` / `scheme` 字段本仓当前不消费（rust 端 bucket / region 直传，前端走
//! > nginx 同源），保留 serde 字段映射方便后续联调。
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

/// python 端 STS 凭证块（嵌套在 [`PythonStsCredential::credentials`] 下）。
///
/// 字段对齐 `backend-python/schema/sts.py::StsCredentialsOut`：凭证三件 + 起止时间。
///
/// 2026-09-18 修复 review 第 2 轮：原实现把所有字段铺平在 `PythonStsCredential`
/// 一级，但 python 实际 schema 是嵌套 `data.credentials.{tmp_secret_id,...}` →
/// serde 字段全 miss → 解析失败 → 21608 + body_preview=整段嵌套 JSON。
///
/// `Default` derive 理由：serde `#[serde(default)]` 在 `Option<T>` 字段上要求
/// `T: Default`（虽然理论上 `Option::default() = None` 不需要 inner default，但
/// serde derive 强制要求 inner `T: Default`）。本结构**只**用于 serde 反序列化
/// 容错场景——`PythonEnvelope.data: Option<PythonStsCredential>` 缺字段 / `null`
/// 时退化为 `Some(default())` / `None`，调用方拿到后做语义校验；**不**用于
/// 业务构造（业务构造走显式字段 init）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub tmp_secret_id: String,
    pub tmp_secret_key: String,
    pub session_token: String,
    pub start_time: i64,
    pub expired_time: i64,
}

/// 单次签发的 STS 凭证（python 端 `StsPrefixCredentialsResponse` → rust 内）。
///
/// 字段对齐 `backend-python/schema/sts.py:118-127`：凭证块嵌在 `credentials` 子对象，
/// 顶层另有 `start_time` / `expired_time` / `expires_in` / `bucket` / `region` /
/// `endpoint` / `scheme`。`tmp_prefix` **不**回传——rust 调用方 `issue(prefix, ...)`
/// 时已收 prefix 入参，由调用方自行持有（如 `UploadSessionService` 写入
/// `session.tmp_prefix`）。
///
/// 2026-09-18 新增；2026-09-18 第 2 轮修复：扁平 → 嵌套对齐 python schema。
///
/// `Default` derive 理由同上（serde `#[serde(default)]` 兼容性）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PythonStsCredential {
    pub credentials: Credentials,
    pub start_time: i64,
    pub expired_time: i64,
    pub expires_in: i64,
    /// COS bucket（python 端负责从配置读，rust 不再持 bucket 配置）。
    pub bucket: String,
    /// COS region（同上）。
    pub region: String,
    /// COS endpoint（python 端拼好后回传；本仓当前不消费，仅保留字段映射）。
    pub endpoint: String,
    /// COS scheme（`"https"` / `"http"`；同上，仅保留映射）。
    pub scheme: String,
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
/// 2026-09-18 新增；2026-09-18 第 2 轮 review 修复：`data` 加 `#[serde(default)]`
/// —— python middleware 在系统异常 / 旧版未升级时可能省略 `data` 字段（不只
/// `null`），原实现在那种 body 下会整个信封 serde 失败 → 21608。
/// `Option<T>` + `default` 双重保险：`null` / 缺字段 / 字段类型错 三种情况都
/// 能落到本层语义校验，而不是早死。
///
/// rust 端必须**先**反序列化为此层，再判断 `code` / 取 `data`——
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
    /// 实际响应体。成功时是 `T`；业务异常时通常是 `null`；python 旧版或
    /// 系统异常时可能**整字段缺失**。`#[serde(default)]` 让"缺字段"与
    /// `"null"` 行为对齐（都进 `None` 分支）。
    #[serde(default)]
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
        _prefix: &str,
        _expire_seconds: u32,
    ) -> Result<PythonStsCredential, AppError> {
        // 占位实现给个 1h 过期；prefix 由 caller 持有（不再回传到 `PythonStsCredential`，
        // 因 python schema 不返回 tmp_prefix）。bucket/region 固定占位；
        // endpoint/scheme 同样用占位串，便于后续切换真实实现时字段对齐。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(PythonStsCredential {
            credentials: Credentials {
                tmp_secret_id: "AKIDnoop".to_string(),
                tmp_secret_key: "noop".to_string(),
                session_token: "noop-token".to_string(),
                start_time: now,
                expired_time: now + 3600,
            },
            start_time: now,
            expired_time: now + 3600,
            expires_in: 3600,
            bucket: "noop-bucket".to_string(),
            region: "ap-shanghai".to_string(),
            endpoint: "https://cos.ap-shanghai.myqcloud.com".to_string(),
            scheme: "https".to_string(),
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
    // 2026-09-18 新增；2026-09-18 第 2 轮 review 修复：fixture 全部改用 python
    // 实际返回的嵌套 schema（对齐 backend-python/service/sts.py::grant_prefix_credentials
    // 经 UnifiedResponseMiddleware 包装的产物）。
    // ============================================================

    /// python 后端 `service/sts.py::grant_prefix_credentials` 经
    /// `UnifiedResponseMiddleware` 包装的**真实**成功响应 body（去掉 envelope 外壳）。
    ///
    /// 2026-09-18 第 2 轮 review 修复：原 fixture 用扁平结构，与 python 实际 schema
    /// 严重不符，导致所有成功调用仍 21608。本常量直接抄 reviewer 给出的真实示例。
    const PYTHON_STS_RESPONSE_DATA_OK: &str = r#"{
  "credentials": {
    "tmp_secret_id": "AKID-real",
    "tmp_secret_key": "sk-real",
    "session_token": "tok-real",
    "start_time": 1734567890,
    "expired_time": 1734571490
  },
  "start_time": 1734567890,
  "expired_time": 1734571490,
  "expires_in": 3600,
  "bucket": "myerp-prod-1300000000",
  "region": "ap-shanghai",
  "endpoint": "https://cos.ap-shanghai.myqcloud.com",
  "scheme": "https"
}"#;

    /// 拼一个完整的 python 信封 JSON 字符串（含 data）。
    /// `code=0` + 完整 `data`（嵌套结构，对齐 python schema）→ 解析成功 +
    /// `code==0` + `Some(cred)` 三条件全满足。
    #[test]
    fn parse_python_success_envelope() {
        // envelope 包裹 + 真实 data 形状（嵌套 credentials + 顶层其它字段）。
        let data_json = PYTHON_STS_RESPONSE_DATA_OK;
        let body = format!(r#"{{"code":0,"message":"ok","data":{data_json}}}"#);
        let cred =
            parse_python_response(StatusCode::OK, body.as_bytes()).expect("完整信封应解析成功");
        // 嵌套 credentials 字段
        assert_eq!(cred.credentials.tmp_secret_id, "AKID-real");
        assert_eq!(cred.credentials.tmp_secret_key, "sk-real");
        assert_eq!(cred.credentials.session_token, "tok-real");
        assert_eq!(cred.credentials.start_time, 1_734_567_890);
        assert_eq!(cred.credentials.expired_time, 1_734_571_490);
        // 顶层字段
        assert_eq!(cred.start_time, 1_734_567_890);
        assert_eq!(cred.expired_time, 1_734_571_490);
        assert_eq!(cred.expires_in, 3600);
        assert_eq!(cred.bucket, "myerp-prod-1300000000");
        assert_eq!(cred.region, "ap-shanghai");
        assert_eq!(cred.endpoint, "https://cos.ap-shanghai.myqcloud.com");
        assert_eq!(cred.scheme, "https");
    }

    /// 2026-09-18 第 2 轮 review 修复：直接喂 python 真实 envelope（含 code / message /
    /// 嵌套 data），断言整链路解析成功（这是 21608 修复的**核心**测试——之前所有成功
    /// 调用都因扁平 schema 对不上嵌套 schema 解析失败）。
    #[test]
    fn parse_python_real_envelope_roundtrip() {
        let body = format!(r#"{{"code":0,"message":"ok","data":{PYTHON_STS_RESPONSE_DATA_OK}}}"#);
        let cred = parse_python_response(StatusCode::OK, body.as_bytes())
            .expect("python 真实 envelope 应一次解析成功（21608 修复核心断言）");
        // 校验关键字段映射（不是只看不报错）
        assert_eq!(cred.credentials.tmp_secret_id, "AKID-real");
        assert_eq!(cred.credentials.tmp_secret_key, "sk-real");
        assert_eq!(cred.credentials.session_token, "tok-real");
        assert_eq!(cred.bucket, "myerp-prod-1300000000");
        assert_eq!(cred.region, "ap-shanghai");
        assert_eq!(cred.expires_in, 3600);
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

    /// 2026-09-18 第 2 轮 review 修复：`data` 字段**整字段缺失**（python middleware
    /// 旧版或系统异常路径可能省略 `data`，不只发 `null`）。`#[serde(default)]` 应让
    /// 此场景进 `data == None` 分支而不是早死。
    #[test]
    fn parse_python_envelope_missing_data_field() {
        // 注意：故意不写 "data" key
        let body = br#"{"code":0,"message":"ok"}"#;
        let err = parse_python_response(StatusCode::OK, body)
            .expect_err("data 字段缺失应走 data 为空分支");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
        assert!(
            err.to_string().contains("data 为空"),
            "data 字段缺失应进 'data 为空' 分支（不是 malformed 解析失败）"
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

    /// 2026-09-18 第 2 轮 review 修复：body 含非 UTF-8 字节 → 解析失败分支
    /// 不应 panic（`String::from_utf8_lossy` 对非法 UTF-8 用 U+FFFD 替换，
    /// 必须确保不会触发 unwrap / from_utf8 等会 panic 的路径）。
    #[test]
    fn parse_python_non_utf8_body_does_not_panic() {
        // 构造含非 UTF-8 字节的"看起来像 JSON"的 body（缺右括号 + 0xFF/0xFE/0xFD）。
        // serde_json 一定会报 malformed（不是合法 JSON），但关键是 parse_python_response
        // 不 panic；错误消息含 body_preview，preview 走 from_utf8_lossy 替换。
        let body: &[u8] = b"\xff\xfe\xfd{\"code\":0,\"message\":\"ok\"";
        let err = parse_python_response(StatusCode::BAD_GATEWAY, body)
            .expect_err("非 UTF-8 / malformed JSON 应失败");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED);
        let msg = err.to_string();
        assert!(
            msg.contains("响应解析失败"),
            "错误消息应指出解析失败: {msg}"
        );
        assert!(
            msg.contains("body_preview="),
            "错误消息应含 body_preview（from_utf8_lossy 不 panic 即视为通过）: {msg}"
        );
        // 不强断言含特定子串（U+FFFD 转码后是 �，但平台 / 字符串实现可能有差异）；
        // 关键是 "没 panic + 错误消息字符串能正常拼出来"。
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

    /// `code=0` 但 `data` 嵌套结构缺字段（python 端契约漂移 / 字段漏传）→ Err
    /// （serde 缺字段 → 整信封反序列化失败 → 走解析失败分支）。消息含 body 预览。
    #[test]
    fn parse_python_wrong_shape_data() {
        // 故意缺 credentials.tmp_secret_key（嵌套块里少字段）
        let body = br#"{"code":0,"message":"ok","data":{"credentials":{"tmp_secret_id":"x","session_token":"t","start_time":1,"expired_time":2},"start_time":1,"expired_time":2,"expires_in":1,"bucket":"b","region":"r","endpoint":"e","scheme":"https"}}"#;
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
    // 既有单测（Noop + serde roundtrip）
    // 2026-09-18 第 2 轮修复：改用嵌套结构构造 + 验证
    // ============================================================

    #[tokio::test]
    async fn noop_python_sts_returns_valid_placeholder() {
        let s = NoopPythonSts;
        let cred = s.issue("tmp/sess/abc/", 3600).await.expect("Noop 不抛错");
        // 嵌套 credentials 字段
        assert_eq!(cred.credentials.tmp_secret_id, "AKIDnoop");
        assert_eq!(cred.credentials.tmp_secret_key, "noop");
        assert_eq!(cred.credentials.session_token, "noop-token");
        // 顶层字段
        assert!(cred.expired_time > cred.start_time);
        assert_eq!(cred.expires_in, 3600);
        assert_eq!(cred.bucket, "noop-bucket");
        assert_eq!(cred.region, "ap-shanghai");
        assert_eq!(cred.scheme, "https");
    }

    /// `PythonStsCredential` 嵌套结构 serde roundtrip：对齐全嵌套字段
    /// （嵌套 `Credentials` + 顶层 7 字段）。
    #[test]
    fn python_sts_credential_roundtrips_json() {
        let c = PythonStsCredential {
            credentials: Credentials {
                tmp_secret_id: "id".into(),
                tmp_secret_key: "key".into(),
                session_token: "tok".into(),
                start_time: 100,
                expired_time: 3700,
            },
            start_time: 100,
            expired_time: 3700,
            expires_in: 3600,
            bucket: "b".into(),
            region: "r".into(),
            endpoint: "https://cos.ap-shanghai.myqcloud.com".into(),
            scheme: "https".into(),
        };
        let json = serde_json::to_string(&c).expect("serialize");
        let back: PythonStsCredential = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.credentials.tmp_secret_id, "id");
        assert_eq!(back.credentials.expired_time, 3700);
        assert_eq!(back.expires_in, 3600);
        assert_eq!(back.scheme, "https");
    }
}
