//! user 域数据模型
//!
//! 对应 Python myERP/model/user.py + model/menu.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//! - 域枚举（DB 用 varchar，应用层用 `crate::auth::rbac::Role` 校验）
//!
//! 说明：`t_role_menu` 不建模——本域只做 `t_menu JOIN t_role_menu` 的读查询，
//! 没有单表读需求，故省略行结构。

use chrono::NaiveDateTime;

/// `t_user` 行
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub full_name: String,
    pub phone: Option<String>,
    pub is_active: bool,
    pub last_login_at: Option<NaiveDateTime>,
    pub refresh_token_version: i32,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_user_role` 行
///
/// `scope_type` / `scope_id` 仅在 `role = SHELF_ACCOUNT` 时非空（scope_type 固定 `shelf`）。
/// 唯一索引 `uk_t_user_role_scope` 是 partial unique（`WHERE deleted_at IS NULL`），
/// 因此软删后可重新添加同一角色。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRole {
    pub id: i64,
    pub user_id: i64,
    pub role: String,
    pub scope_type: Option<String>,
    pub scope_id: Option<i64>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_menu` 行（`parent_id` 自引用，CHECK `ck_t_menu_no_self_loop` 禁止自环）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Menu {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub code: String,
    pub title: String,
    pub path: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_shelf` 行（本域只读，用于 SHELF_ACCOUNT 角色的 scope 校验）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Shelf {
    pub id: i64,
    pub code: String,
    pub name: String,
    /// `PRODUCTION` / `INSPECTION`
    pub zone: String,
    pub location: Option<String>,
    pub is_active: bool,
    pub display_order: i32,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}
