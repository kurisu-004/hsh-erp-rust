//! iam 域 session 端点响应 VO

use serde::Serialize;

use super::menu::MenuNodeOut;

/// `/iam/me` 出参：当前用户 + 扁平角色名 + 权限点 code 全集 + 可访问货架 + 菜单树
#[derive(Debug, Clone, Serialize)]
pub struct CurrentUserOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub username: String,
    pub full_name: String,
    pub is_active: bool,
    pub roles: Vec<String>,
    /// 权限点 code 全集（2026-10-14 新增，与 `roles` / `menus` 并列的账号能力视图；
    /// 前端 `hasPermission` 的输入）。ADMIN 恒空 vec —— 通配在
    /// `require_permission` 代码里，不靠数据。
    pub permissions: Vec<String>,
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
