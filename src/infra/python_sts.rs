//! 转发 python 后端签发 STS 临时凭证
//!
//! 2026-09-18 新增。背景：
//! - 原 backend-rust `infra::sts::TencentSts` 直连腾讯云 `sts.tencentcloudapi.com`，
//!   绕过 python 后端的凭据管理 + 权限审计 + 限流。
//! - 新设计：rust 后端通过 HTTP 转发到 python 端内部端点
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
//! ## 响应体格式（python 端契约，2026-09-18 起草）
//! ```jsonc
//! {
//!   "tmp_secret_id": "...",
//!   "tmp_secret_key": "...",
//!   "session_token": "...",
//!   "start_time": 1734567890,    // unix 秒
//!   "expired_time": 1734571490,  // unix 秒
//!   "bucket": "...",
//!   "region": "...",
//!   "tmp_prefix": "tmp/sess/<uuid>/"
//! }
//! ```
//!
//! 注：python 端响应字段名待 python 子模块 PR 合入后最终确认；本模块按上述契约
//! 实现解析。

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
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

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED,
                format!("python STS 返回 {status}: {body}"),
            ));
        }

        let cred: PythonStsCredential = resp.json().await.map_err(|e| {
            AppError::biz(
                code::BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED,
                format!("python STS 响应 JSON 解析失败: {e}"),
            )
        })?;
        Ok(cred)
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
