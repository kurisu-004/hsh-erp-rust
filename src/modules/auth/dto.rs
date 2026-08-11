//! auth 域 DTO
//!
//! 对应 Python myERP/schema/auth.py（LoginReq / TokenPairResp / RefreshReq / LogoutResp）。
//!
//! ## 字段名硬约定
//! `LoginResponse` 的字段必须严格是 `token` / `refresh_token` / `user`，对齐 Python 前端
//! 已绑定的字段。`ChangePasswordRequest` 由 user 域提供，auth handler 直接复用。

use serde::{Deserialize, Serialize};

use crate::modules::user::dto::CurrentUserOut;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// 登录与 refresh 的统一响应（含 access + refresh token + 用户信息）
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub refresh_token: String,
    pub user: CurrentUserOut,
}

/// refresh token 换新 access/refresh pair
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// 登出结果（no-op，前端清除本地 token 即视为登出）
#[derive(Debug, Serialize)]
pub struct LogoutResponse {
    pub ok: bool,
}
