//! STS 临时凭证签发（前端直传 COS 用）
//!
//! 2026-09-18 修复 review #2：rust 直连 `cos_rust_sdk::sts` 链路已由
//! `infra::python_sts::HttpPythonSts` 替代（python 后端统一管 STS 凭据 / 审计 / 限流）。
//! 但**保留本文件 + NoopSts 占位**，原因：
//!
//! 1. **对齐 `infra::cos::NoopCos`** —— 两个 Noop 实现（NoopCos + NoopPythonSts）
//!    是 trait 注入模式的对偶，保留 NoopSts 让"本地 cargo run 不依赖外部凭据"模式
//!    的接口对称。
//! 2. **`BIZ_STS_ISSUE_FAILED` (21116) 错误码对齐 Python 错误码表** —— Python 端
//!    STS 失败统一抛 21116，前端可能依此路由（如展示"STS 签发失败，请重试"）。
//!    保留常量 + status_from_code 行供未来若 python 端临时降级回直连 SDK 时复用。
//!
//! 因此本文件**仅保留 NoopSts**，不再保留 `TencentSts`（其依赖的 `cos_rust_sdk::sts`
//! 链路已废弃）。`BIZ_STS_ISSUE_FAILED` 常量 / status_from_code 行 / 测试断言
//! 全部由 `src/shared/error.rs` 持有。
//!
//! ## 历史
//! - 2026-09-16 M2-A 新增：`TencentSts`（直连腾讯云）+ `NoopSts` 占位
//! - 2026-09-18 M3-B：`TencentSts` 删除，迁至 `infra::python_sts::HttpPythonSts`
//!   经 python 后端 HTTP 转发签发 STS
//! - 2026-09-18 review #2：本文件保留为 `NoopSts` 占位（与 `NoopCos` 对偶）

use async_trait::async_trait;

use crate::shared::error::AppError;

/// 2026-09-16 M2-A 新增：STS 临时凭证（前端直传 COS 用）。
///
/// 字段命名贴近 SDK `TemporaryCredentials`，但 `expired_time` 用 `i64` 而非 `Option<u64>`
/// 是为了与现有 DTO（雪花 ID i64 / `i64` unix 秒）一致；JSON 序列化走 `to_string`。
///
/// 2026-09-18：保留 `pub` 字段以兼容潜在外部调用方；本模块不导出 SDK 直连实现。
#[derive(Debug, Clone)]
pub struct StsCredential {
    pub tmp_secret_id: String,
    pub tmp_secret_key: String,
    pub session_token: String,
    /// Unix 秒（UTC）。SDK 返回 `Option<u64>`，缺省回退到「now + duration_seconds」。
    pub expired_time: i64,
    /// COS bucket 名（前端拼 endpoint / x-cos-security-token 等）。
    pub bucket: String,
    /// COS region。
    pub region: String,
    /// 写入 prefix（前端拼 object key 前缀），含尾斜杠。
    pub tmp_prefix: String,
}

/// 凭证签发 trait。
///
/// 2026-09-18：保留 trait 接口但**实际生产路径**走 `infra::python_sts::PythonSts`；
/// 本 trait 仅作为 Noop 占位的接口声明存在，便于未来若 python 端临时降级回
/// 直连 SDK 时复用类型签名。
#[async_trait]
pub trait StsCredentialIssuer: Send + Sync {
    async fn issue(
        &self,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
    ) -> Result<StsCredential, AppError>;

    /// 2026-09-16 M2-B 新增：批量场景签发（upload-intents 端点用）。
    ///
    /// `tmp_sub_prefix`：caller 拼好的 tmp 前缀（不含 kind），policy 覆盖 `tmp_sub_prefix/*`。
    async fn issue_for_intents(&self, tmp_sub_prefix: &str) -> Result<StsCredential, AppError>;
}

/// `COS_ENABLED=false` 或 `PYTHON_BACKEND_BASE_URL` 空时的占位实现（与 `NoopCos`
/// 对偶）：返回合法结构体 + 长过期时间，便于本地 `cargo run` 不依赖真实 STS 凭据
/// 也能跑通上层业务（前端拿到 local 占位 token 不会真去上传，只走前端骨架）。
///
/// 2026-09-18 review #2：本占位是 sts.rs 仅有的实现；TencentSts 已迁至 python_sts 链路。
pub struct NoopSts;

#[async_trait]
impl StsCredentialIssuer for NoopSts {
    async fn issue(
        &self,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
    ) -> Result<StsCredential, AppError> {
        let full_prefix = format!("tmp/{owner_kind}/{owner_id}/{kind}/");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(StsCredential {
            tmp_secret_id: "AKIDnoop".to_string(),
            tmp_secret_key: "noop".to_string(),
            session_token: "noop-token".to_string(),
            expired_time: now + 3600,
            bucket: "noop-bucket".to_string(),
            region: "ap-shanghai".to_string(),
            tmp_prefix: full_prefix,
        })
    }

    /// 2026-09-16 M2-B 新增：批量场景 STS 凭证签发（Noop 占位实现）。
    ///
    /// `tmp_sub_prefix` 不强制要求 caller 补尾斜杠；NoopSts 内部规范化。
    async fn issue_for_intents(&self, tmp_sub_prefix: &str) -> Result<StsCredential, AppError> {
        let full_prefix = if tmp_sub_prefix.ends_with('/') {
            tmp_sub_prefix.to_string()
        } else {
            format!("{tmp_sub_prefix}/")
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(StsCredential {
            tmp_secret_id: "AKIDnoop".to_string(),
            tmp_secret_key: "noop".to_string(),
            session_token: "noop-token".to_string(),
            expired_time: now + 3600,
            bucket: "noop-bucket".to_string(),
            region: "ap-shanghai".to_string(),
            tmp_prefix: full_prefix,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_sts_returns_valid_placeholder() {
        let issuer = NoopSts;
        let cred = issuer
            .issue("part", 123, "drawing")
            .await
            .expect("NoopSts 不抛错");
        assert_eq!(cred.tmp_prefix, "tmp/part/123/drawing/");
        assert!(cred.expired_time > 0);
    }

    /// 2026-09-16 M2-B 新增：NoopSts::issue_for_intents 规范化 tmp_sub_prefix 尾斜杠
    /// 并返回合法占位凭证。prefix 自动补齐的逻辑与 TencentSts 实现一致。
    #[tokio::test]
    async fn noop_sts_issue_for_intents_normalizes_trailing_slash() {
        let issuer = NoopSts;

        // 不带尾斜杠：自动补
        let cred = issuer
            .issue_for_intents("tmp/batch-uuid")
            .await
            .expect("NoopSts 不抛错");
        assert_eq!(cred.tmp_prefix, "tmp/batch-uuid/");
        assert!(cred.expired_time > 0);

        // 已带尾斜杠：原样
        let cred2 = issuer
            .issue_for_intents("tmp/part/123/")
            .await
            .expect("NoopSts 不抛错");
        assert_eq!(cred2.tmp_prefix, "tmp/part/123/");
    }
}
