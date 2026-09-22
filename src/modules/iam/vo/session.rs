//! iam 域 session 端点响应 VO

use serde::Serialize;

use super::menu::MenuNodeOut;

/// `/iam/me` 出参：当前用户 + 扁平角色名 + 可访问货架 + 菜单树
#[derive(Debug, Clone, Serialize)]
pub struct CurrentUserOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub username: String,
    pub full_name: String,
    pub is_active: bool,
    pub roles: Vec<String>,
    pub shelf_ids: Vec<String>,
    pub menus: Vec<MenuNodeOut>,
}

/// 登录与 refresh 的统一响应（含 access + refresh token + 用户信息）
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub refresh_token: String,
    pub user: CurrentUserOut,
}

/// 登出结果（no-op，前端清除本地 token 即视为登出）
#[derive(Debug, Serialize)]
pub struct LogoutResponse {
    pub ok: bool,
}