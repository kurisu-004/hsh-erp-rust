//! RBAC 子模块数据模型（`t_role` / `t_permission` 行投影）
//!
//! 三张表里只有前两张需要行投影：`t_role_permission` 的读侧一律是
//! 「取一批 code 字符串」/「批量插行」，没有逐字段消费方，故不建 FromRow 结构。
//!
//! 审计列（`created_at` / `created_by` / `updated_at` / `updated_by` /
//! `deleted_at`）照 `t_menu` 逐列一致，本文件保留它们是为了 `version_conflict`
//! 之类的写侧归因与出参回显；`id` 等价于其它表的雪花主键（App 侧生成）。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// `t_role` 行
#[derive(Debug, Clone, FromRow)]
pub struct TRole {
    pub id: i64,
    /// 角色码（大写 varchar(20)）。**无 FK** 指向本表 —— `t_user_role.role` /
    ///   `t_role_menu.role` / `t_role_permission.role_code` 都是同源的 varchar。
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    /// 内置角色标记。`true` ⇒ code 不可改、不可删除（一般只可停用；
    /// ADMIN / MANAGER 连停用也固定 400）。
    pub is_system: bool,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

#[cfg(test)]
impl TRole {
    /// 单测用最小行构造（service 单测 mock `get_active_role_by_code` 的返回值）。
    pub fn dummy(code: &str) -> Self {
        let now =
            NaiveDateTime::parse_from_str("2026-10-14T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        Self {
            id: 1,
            code: code.to_string(),
            name: "测试角色".into(),
            description: None,
            is_system: false,
            is_active: true,
            version: 0,
            created_at: now,
            created_by: None,
            updated_at: now,
            updated_by: None,
            deleted_at: None,
        }
    }
}

/// `t_permission` 行
#[derive(Debug, Clone, FromRow)]
pub struct TPermission {
    pub id: i64,
    /// 权限码（点分，如 `part.drawing.print`）。
    pub code: String,
    pub name: String,
    /// 业务域（`code` 首段）；`GET /permissions` 按此分组。
    pub domain: String,
    pub description: Option<String>,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}
