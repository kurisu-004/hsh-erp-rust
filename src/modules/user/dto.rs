//! user 域 DTO
//!
//! 对应 Python myERP/schema/user.py。命名约定：
//! - `XxxRequest`：写操作入参
//! - `XxxOut`：出参（雪花 id 序列化为 JSON string，防 JS 精度截断）
//! - `XxxListOut`：列表分页
//! - `XxxListQuery`：列表查询参数
//!
//! ## id 序列化约定
//! 裸 `i64` 字段用 `#[serde(serialize_with = "crate::shared::types::serialize_i64")]`。
//! 可空 id（`scope_id` / `parent_id`）与 id 列表（`shelf_ids`）在 service 层就转成
//! `Option<String>` / `Vec<String>`，避免为 `Option<i64>` 再写一套 serde helper——
//! 出参 JSON 形态与 Python 完全一致（null 仍是 null）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::auth::rbac::Role;

// ---------------------------------------------------------------------------
// 出参
// ---------------------------------------------------------------------------

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

/// 菜单树节点（递归 children）
#[derive(Debug, Clone, Serialize)]
pub struct MenuNodeOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub parent_id: Option<String>,
    pub code: String,
    pub title: String,
    pub path: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
    #[serde(default)]
    pub children: Vec<MenuNodeOut>,
}

/// `/auth/me` 出参：当前用户 + 扁平角色名 + 可访问货架 + 菜单树
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

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct UserCreateRequest {
    pub username: String,
    pub password: String,
    pub full_name: String,
    #[serde(default)]
    pub phone: Option<String>,
}

/// 部分更新：字段为 `None` 表示不修改（与 Python `exclude_unset` 语义一致）
#[derive(Debug, Clone, Deserialize)]
pub struct UserUpdateRequest {
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserListQuery {
    #[serde(default)]
    pub username_like: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// 添加角色入参。`role` 反序列化自大写字符串（MANAGER/CLERK/...，见 `auth::rbac::Role`）。
/// SHELF_ACCOUNT 必须带 `scope_type = "shelf"` + `scope_id`；其余角色两者必须为空。
#[derive(Debug, Clone, Deserialize)]
pub struct UserAddRoleRequest {
    pub role: Role,
    #[serde(default)]
    pub scope_type: Option<String>,
    #[serde(default)]
    pub scope_id: Option<i64>,
}

/// 自助/管理员改密入参（旧密码在自助改密时必填）
#[derive(Debug, Clone, Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}
