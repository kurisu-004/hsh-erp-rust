//! 腾讯云 COS 客户端抽象
//!
//! 对应 Python myERP/core/cos.py：
//! - `put_object` / `get_object` / `presigned_get_url` / `delete_object`
//! - 业务实现阶段补：`upload_file_advanced`（大文件分块）、`download_object_cached`（SHA256 LRU 缓存）
//!
//! 业务实现策略：使用 `reqwest` 自行实现 COS XML API 签名（HMAC-SHA1），
//! 不依赖官方 SDK；`NoopCos` 作为骨架阶段占位。

use async_trait::async_trait;

use crate::shared::error::{code, AppError};

#[async_trait]
pub trait CosClient: Send + Sync {
    async fn put_object(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<(), AppError>;
    async fn get_object(&self, key: &str) -> Result<Vec<u8>, AppError>;
    async fn presigned_get_url(&self, key: &str, expires_seconds: u32) -> Result<String, AppError>;
    async fn delete_object(&self, key: &str) -> Result<(), AppError>;
}

/// 骨架阶段占位实现：所有调用返回业务错误
pub struct NoopCos;

#[async_trait]
impl CosClient for NoopCos {
    async fn put_object(
        &self,
        _key: &str,
        _body: Vec<u8>,
        _content_type: &str,
    ) -> Result<(), AppError> {
        Err(AppError::biz(
            code::INTERNAL,
            "COS 客户端未配置（骨架阶段），实施阶段注入实现",
        ))
    }

    async fn get_object(&self, _key: &str) -> Result<Vec<u8>, AppError> {
        Err(AppError::biz(code::INTERNAL, "COS 客户端未配置"))
    }

    async fn presigned_get_url(
        &self,
        _key: &str,
        _expires_seconds: u32,
    ) -> Result<String, AppError> {
        Err(AppError::biz(code::INTERNAL, "COS 客户端未配置"))
    }

    async fn delete_object(&self, _key: &str) -> Result<(), AppError> {
        Err(AppError::biz(code::INTERNAL, "COS 客户端未配置"))
    }
}