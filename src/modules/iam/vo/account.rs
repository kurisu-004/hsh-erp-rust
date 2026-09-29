//! iam 域 account 端点响应 VO

use chrono::NaiveDateTime;
use serde::Serialize;

/// 用户角色出参。`shelf_code` / `shelf_name` 仅 SHELF_ACCOUNT 角色非空。
#[derive(Debug, Clone, Serialize)]
pub struct UserRoleOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub role: String,
    pub scope_type: Option<String>,
    pub scope_id: Option<String>,
    pub shelf_code: Option<String>,
    pub shelf_name: Option<String>,
}

/// 用户详情出参（含角色列表）
#[derive(Debug, Clone, Serialize)]
pub struct UserOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub username: String,
    pub full_name: String,
    pub phone: Option<String>,
    pub is_active: bool,
    pub last_login_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub roles: Vec<UserRoleOut>,
}

/// 用户列表出参。
///
/// 字段与顺序对齐 Python `schema/user.py::UserListOut`——即 `items, total, limit, offset`
/// 四个字段。前端会回显 `limit`/`offset` 做翻页，故不裁剪为 `{total, items}`；
/// 也因此不直接复用 `shared::response::Page<T>`（那是只有 total+items 的通用结构）。
#[derive(Debug, Clone, Serialize)]
pub struct UserListOut {
    pub items: Vec<UserOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 单条企业微信绑定出参（2026-09-29 新增，`GET /iam/users/{id}/wx-bind` 的 `data[]`）
///
/// ⚠️ **不含** `corpsecret` / `session_key` —— 本表也从不存这两样
/// （`session_key` 拿到即丢，见 `wx/wecom_client.rs` 模块 doc）。
#[derive(Debug, Clone, Serialize)]
pub struct WxIdentityOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub corp_id: String,
    /// 企业微信 userid（已归一化为小写）
    pub wx_user_id: String,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub user_id: i64,
    pub version: i32,
    pub created_at: NaiveDateTime,
}