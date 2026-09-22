//! iam 域 session 端点入参（公开端点 login / refresh）

use serde::Deserialize;

/// POST /api/v2/iam/login 入参
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// POST /api/v2/iam/refresh 入参
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}