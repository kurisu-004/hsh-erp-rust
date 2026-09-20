//! 腾讯云 COS 客户端抽象（trait + ObjectMeta + NoopCos 占位）
//!
//! 对应 Python `backend-python/core/cos.py`：
//! - `put_object` / `get_object` / `presigned_get_url` / `delete_object`
//! - M2 新增 `head_object` / `copy_object`（服务端→服务端 copy + 元数据查询）
//! - 业务实现阶段补：`upload_file_advanced`（大文件分块）、`download_object_cached`（SHA256 LRU 缓存）
//!
//! 2026-09-11 重构：使用 `cos-rust-sdk`（社区 SDK，仿 cos-python-sdk-v5）作为
//! 上传 / 下载 / 删除的真实客户端；`presigned_get_url` SDK 未提供，单独用
//! `hmac` + `sha1` + `hex` 手写 V1 签名拼 URL。新增 `COS_ENABLED` 开关：
//! 关闭时使用 `NoopCos` 静默成功，便于本地 `cargo run` 不依赖真实凭据。
//!
//! 2026-09-16 M2-A 重构签名算法为两步 HMAC（hex SignKey → hex Signature），
//! 与 cos-rust-sdk 一致；移除 `base64` 依赖。同时新增 `head_object` / `copy_object`。
//!
//! 2026-09-20 迁移：清掉 `cos-rust-sdk` + `Presigner` + `sha1_hex` / `hmac_sha1_*` /
//! `percent_encode_*` / 已知向量测试（净减 ~325 行）。真实 COS 客户端迁至
//! `infra::cos_opendal::OpenDalCos`（Apache OpenDAL S3 backend），presign 改走 S3 v4。
//! 本文件仅保留 `CosClient` trait / `ObjectMeta` / `NoopCos` 占位实现。
//!
//! V1 签名算法（仅留历史注释，2026-09-20 已删除）参考：
//! ```text
//! HttpRequestInfo = method\n + pathname_encoded\n + params_string\n + headers_string\n
//! StringToSign    = sha1\n + sign_time\n + sha1_hex(HttpRequestInfo)\n
//! SignKey         = HMAC-SHA1(SecretKey, KeyTime)         → hex (40 chars)
//! Signature       = HMAC-SHA1(SignKey hex bytes, StringToSign) → hex (40 chars)
//! ```

use async_trait::async_trait;
use tracing::info;

use crate::shared::error::AppError;

/// COS 对象元数据（head_object 返回值）。
///
/// 2026-09-16 M2-A 新增：M3 业务模块需要校验服务端 tmp_object 存在 + 大小匹配。
/// `size` 选 i64 而非 u64 是为了与 `t_part_file.size_bytes` 等业务表字段直接比对
/// （避免 usize 跨界转换）。
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub size: i64,
    pub etag: String,
}

#[async_trait]
pub trait CosClient: Send + Sync {
    async fn put_object(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<(), AppError>;
    async fn get_object(&self, key: &str) -> Result<Vec<u8>, AppError>;
    async fn presigned_get_url(&self, key: &str, expires_seconds: u32) -> Result<String, AppError>;
    async fn delete_object(&self, key: &str) -> Result<(), AppError>;
    /// 2026-09-16 M2-A 新增：取对象 size + etag（不带 body）。
    async fn head_object(&self, key: &str) -> Result<ObjectMeta, AppError>;
    /// 2026-09-16 M2-A 新增：服务端→服务端 copy（不下载再上传）。
    async fn copy_object(&self, src_key: &str, dst_key: &str) -> Result<(), AppError>;
}

// =====================================================================================
// NoopCos：`COS_ENABLED=false` 时的静默占位实现，本地 cargo run 调试用
// =====================================================================================

/// `COS_ENABLED=false` 时的占位实现：所有方法静默返回成功。
///
/// 2026-09-11 修改：原骨架阶段 NoopCos 所有方法返回错误，本次改为：
/// - `put_object` / `delete_object`：打 info 日志后 `Ok(())`
/// - `get_object`：返回空字节
/// - `presigned_get_url`：返回 `local://{key}` 占位 URL（不会真发请求）
///
/// 2026-09-16 M2-A：`head_object` 返回 `size=0, etag="noop"`；`copy_object` 打 info 后 Ok。
///
/// 2026-09-20 迁移：与 OpenDAL 的 `NoopOpenDal`（真内存 BTreeMap）行为不完全对齐——
/// NoopCos 仍走静默占位语义；NoopOpenDal 走 OpenDAL Memory backend 真内存（put/get/
/// head/copy 全链路可走）。两份 Noop 实现的差异仅在「真实」程度上，trait 抽象对外一致。
pub struct NoopCos;

impl Default for NoopCos {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl CosClient for NoopCos {
    async fn put_object(
        &self,
        key: &str,
        _body: Vec<u8>,
        _content_type: &str,
    ) -> Result<(), AppError> {
        info!(key = %key, "[NoopCos] 跳过真实上传（COS_ENABLED=false，仅本地调试）");
        Ok(())
    }

    async fn get_object(&self, _key: &str) -> Result<Vec<u8>, AppError> {
        Ok(Vec::new())
    }

    async fn presigned_get_url(
        &self,
        key: &str,
        _expires_seconds: u32,
    ) -> Result<String, AppError> {
        Ok(format!("local://{key}"))
    }

    async fn delete_object(&self, key: &str) -> Result<(), AppError> {
        info!(key = %key, "[NoopCos] 跳过真实删除（COS_ENABLED=false，仅本地调试）");
        Ok(())
    }

    async fn head_object(&self, _key: &str) -> Result<ObjectMeta, AppError> {
        // NoopCos 不真发请求，返回占位元数据；M3 校验 size 时不会真的过这里
        // （因为客户端直传会绕过 NoopCos；NoopCos 仅用于本地 cargo run）。
        Ok(ObjectMeta {
            size: 0,
            etag: "noop".to_string(),
        })
    }

    async fn copy_object(&self, src_key: &str, dst_key: &str) -> Result<(), AppError> {
        info!(
            src = %src_key,
            dst = %dst_key,
            "[NoopCos] 跳过真实 copy_object（COS_ENABLED=false，仅本地调试）"
        );
        Ok(())
    }
}
