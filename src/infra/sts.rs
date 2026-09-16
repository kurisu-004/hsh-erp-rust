//! STS 临时凭证签发（前端直传 COS 用）
//!
//! 2026-09-16 M2-A 新增。背景：
//! - 浏览器直传 COS 需 STS 临时凭证，避免前端拿到永久 SecretKey
//! - `cos_rust_sdk::sts::StsClient::get_credentials` 走腾讯云官方
//!   `sts.tencentcloudapi.com/?Action=GetFederationToken` 接口
//!
//! 业务上 STS policy 必须满足：
//! - `Policy::allow_put_object` 默认 actions（含 PutObject / InitiateMultipartUpload /
//!   UploadPart / CompleteMultipartUpload 等）—— 已通过 spike 验证 CAM 接受
//! - **追加** `name/cos:AbortMultipartUpload` action —— 分片上传中途用户取消/失败时清理
//! - **不包含** `name/cos:DeleteObject` —— spike 实测 STS 凭证 DELETE 403 AccessDenied；
//!   业务清理走永久密钥 SDK，不走 STS（见 `infra/cos.rs::CosClient::delete_object`）
//!
//! resource 格式遵循 SDK 默认（`qcs::cos:*:uid/{appid}:prefix//{appid}/{bucket}/{prefix}*`），
//! spike 已验证服务端接受。
//!
//! 凭证有效期：业务建议 900s，与 `COS_PRESIGN_EXPIRE=900` 对齐（见 `infra::config::CosConfig`）；
//! 可通过 `COS_STS_DURATION_SECONDS` env 覆盖。

use async_trait::async_trait;
use cos_rust_sdk::sts::{GetCredentialsRequest, Policy, StsClient};

use crate::infra::config::CosConfig;
use crate::shared::error::{AppError, code};

/// 2026-09-16 M2-A 新增：STS 临时凭证（前端直传 COS 用）。
///
/// 字段命名贴近 SDK `TemporaryCredentials`，但 `expired_time` 用 `i64` 而非 `Option<u64>`
/// 是为了与现有 DTO（雪花 ID i64 / `i64` unix 秒）一致；JSON 序列化走 `to_string`。
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

/// 凭证签发 trait（main.rs 装配时按 `COS_ENABLED` 二选一）。
///
/// 2026-09-16 M2-A 新增：`issue(owner_kind, owner_id, kind)` 三个维度拼接 tmp_prefix：
/// - `owner_kind`：域标识（如 `"part"` / `"assembly"` / `"outsource"`）
/// - `owner_id`：业务表雪花 ID（用 `i64` 而非 string，避免 string 转换）
/// - `kind`：文件类型标识（如 `"drawing"` / `"cnc_program"`）
#[async_trait]
pub trait StsCredentialIssuer: Send + Sync {
    async fn issue(
        &self,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
    ) -> Result<StsCredential, AppError>;
}

/// 真实 STS 凭证签发器（包装 `cos_rust_sdk::sts::StsClient`）。
///
/// 2026-09-16 M2-A 新增。`issue` 每次新建 policy 并调 `get_credentials`；调用频率与
/// 前端直传请求量一致，无本地缓存（缓存层放后续如确有需要再加）。
pub struct TencentSts {
    inner: StsClient,
    bucket: String,
    region: String,
    /// prefix 模板前缀（默认 `tmp/`），最终 prefix = `{tmp_prefix}{owner_kind}/{owner_id}/{kind}/`
    tmp_prefix: String,
    /// 凭证有效期（秒）。spike 实测默认 900s 与 `COS_PRESIGN_EXPIRE` 对齐即可。
    duration_seconds: u32,
}

impl TencentSts {
    /// 构造真实 STS 签发器。
    ///
    /// 2026-09-16 M2-A：从 `CosConfig` 一次性读齐（复用同一个 secret_id/secret_key/region）。
    /// 不显式缓存 `app_id`：`Policy::allow_put_object` 内部按 bucket 末段解析
    /// （SDK 源码 `cos-rust-sdk-0.1.2/src/sts.rs::allow_put_object`）；如果
    /// `COS_APP_ID` 与 bucket 末段不一致，应显式配 `COS_APP_ID`，让 SDK 优先用它。
    pub fn new(cfg: &CosConfig) -> anyhow::Result<Self> {
        // 防御性校验：bucket 必须含 `-`（SDK 解析依赖）；不强制读 cfg.app_id，避免与 SDK 行为分裂。
        if cfg.app_id.is_empty() && cfg.bucket.rsplit_once('-').is_none() {
            anyhow::bail!("无法从 bucket 名解析 app_id（COS_APP_ID 留空且 bucket 不含 '-'）");
        }
        let inner = StsClient::new(
            cfg.secret_id.clone(),
            cfg.secret_key.clone(),
            cfg.region.clone(),
        );
        Ok(Self {
            inner,
            bucket: cfg.bucket.clone(),
            region: cfg.region.clone(),
            tmp_prefix: cfg.tmp_prefix.clone(),
            duration_seconds: cfg.sts_duration_seconds,
        })
    }
}

#[async_trait]
impl StsCredentialIssuer for TencentSts {
    async fn issue(
        &self,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
    ) -> Result<StsCredential, AppError> {
        // 1. 拼 prefix = `{tmp_prefix}{owner_kind}/{owner_id}/{kind}/`
        //    例如 `tmp/part/1234567890/drawing/` —— 前端用此 prefix 写对象
        //    bucket-level 限权，确保跨 owner 互不干扰。
        let full_prefix = format!("{}{}/{}/{}/", self.tmp_prefix, owner_kind, owner_id, kind);

        // 2. 构造 policy：默认 PutObject 系 actions + 追加 AbortMultipartUpload
        //    - `Policy::allow_put_object` 已含 PutObject / PostObject / InitiateMultipartUpload /
        //      ListMultipartUploads / ListParts / UploadPart / CompleteMultipartUpload
        //    - AbortMultipartUpload 必须手动追加（spike 实测 SDK 默认不含，分片上传失败清理会 403）
        //    - 不追加 DeleteObject（STS 凭证 DELETE 403 AccessDenied；业务清理走永久密钥）
        let mut policy = Policy::allow_put_object(&self.bucket, Some(&full_prefix));
        if let Some(stmt) = policy.statement.first_mut() {
            stmt.action
                .push("name/cos:AbortMultipartUpload".to_string());
        }

        // 3. 调 SDK `get_credentials`（内部走 sts.tencentcloudapi.com HTTPS）
        let creds = self
            .inner
            .get_credentials(GetCredentialsRequest {
                policy,
                name: Some(format!("hsh-erp-{}-{}-{}", owner_kind, owner_id, kind)),
                duration_seconds: Some(self.duration_seconds),
            })
            .await
            .map_err(|e| {
                AppError::biz(
                    code::BIZ_STS_ISSUE_FAILED,
                    format!("STS get_credentials 失败: {e}"),
                )
            })?;

        // 4. expired_time 缺省回退到 now + duration_seconds（SDK 旧版响应可能不带）
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let expired_time = creds
            .expired_time
            .map(|e| e as i64)
            .unwrap_or_else(|| now + self.duration_seconds as i64);

        Ok(StsCredential {
            tmp_secret_id: creds.tmp_secret_id,
            tmp_secret_key: creds.tmp_secret_key,
            session_token: creds.token,
            expired_time,
            bucket: self.bucket.clone(),
            region: self.region.clone(),
            tmp_prefix: full_prefix,
        })
    }
}

/// `COS_ENABLED=false` 时的占位实现（与 `NoopCos` 对偶）：返回合法结构体 + 长过期时间，
/// 便于本地 `cargo run` 不依赖真实 STS 凭据也能跑通上层业务（前端拿到 local 占位
/// token 不会真去上传，只走前端骨架）。
///
/// 2026-09-16 M2-A 新增。
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M2-A 关键断言：policy 必须满足业务需求——
    /// - 含默认 PutObject 系 7 个 actions
    /// - 显式追加 `name/cos:AbortMultipartUpload`
    /// - 不含 `name/cos:DeleteObject`（spike 实测 STS DELETE 403）
    /// - resource 列表非空 + effect = "allow"
    #[test]
    fn policy_contains_required_actions_and_no_delete() {
        // 模拟 TencentSts::issue 内的 policy 拼装（不依赖 StsClient 网络）
        let bucket = "erp-drawing-1410882329";
        let prefix = "tmp/part/123/drawing/";
        let mut policy = Policy::allow_put_object(bucket, Some(prefix));
        if let Some(stmt) = policy.statement.first_mut() {
            stmt.action
                .push("name/cos:AbortMultipartUpload".to_string());
        }

        assert_eq!(policy.version, "2.0");
        assert_eq!(
            policy.statement.len(),
            1,
            "只允许 1 个 statement（SDK 默认）"
        );
        let stmt = &policy.statement[0];
        assert_eq!(stmt.effect, "allow");
        assert!(!stmt.resource.is_empty(), "resource 必须非空");
        // 关键 actions 覆盖
        for required in [
            "name/cos:PutObject",
            "name/cos:InitiateMultipartUpload",
            "name/cos:UploadPart",
            "name/cos:CompleteMultipartUpload",
            "name/cos:AbortMultipartUpload",
        ] {
            assert!(
                stmt.action.iter().any(|a| a == required),
                "policy 必须含 action `{required}`，实际 actions = {:?}",
                stmt.action
            );
        }
        // 关键：不含 DeleteObject
        assert!(
            !stmt.action.iter().any(|a| a == "name/cos:DeleteObject"),
            "policy **禁止** 含 `cos:DeleteObject`（STS 凭证 DELETE 403；业务清理走永久密钥）"
        );

        // resource 包含 prefix
        let resource_str = stmt.resource.first().expect("resource 非空");
        assert!(
            resource_str.contains(prefix.trim_end_matches('/')),
            "resource 必须包含 prefix；实际 = {resource_str}"
        );
        assert!(
            resource_str.starts_with("qcs::cos:"),
            "resource 必须符合 SDK 默认 qcs::cos: 格式；实际 = {resource_str}"
        );

        // JSON 序列化能 round-trip
        let json = serde_json::to_string(&policy).expect("policy JSON 序列化");
        assert!(json.contains("\"version\":\"2.0\""));
        assert!(json.contains("\"effect\":\"allow\""));
        assert!(json.contains("name/cos:AbortMultipartUpload"));
    }

    #[test]
    fn full_prefix_layout() {
        // tmp_prefix = "tmp/", owner_kind = "part", owner_id = 123, kind = "drawing"
        // → "tmp/part/123/drawing/"
        let tmp_prefix = "tmp/";
        let full = format!("{tmp_prefix}{}/{}/{}/", "part", 123, "drawing");
        assert_eq!(full, "tmp/part/123/drawing/");
    }

    #[tokio::test]
    async fn noop_sts_returns_valid_placeholder() {
        let issuer = NoopSts;
        let cred = issuer
            .issue("part", 123, "drawing")
            .await
            .expect("NoopSts 不抛错");
        assert_eq!(cred.tmp_prefix, "tmp/part/123/drawing/");
        assert!(cred.expired_time > 0);
        // NoopSts 不报 INTERNAL/INTERNAL_SERVER_ERROR 类的错误
    }
}
