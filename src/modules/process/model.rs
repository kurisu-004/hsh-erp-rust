//! process 域数据模型
//!
//! 对应 Python myERP/model/process.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//! - 域枚举（DB 用 varchar，应用层用 enum 校验）
//!
//! Phase P2 process CRUD 完整列：`id` / `code` / `name` / `category` / `sort_order` /
//! `description` / `version` / `created_*` / `updated_*` / `deleted_at` / `requires_approval`。
//!
//! `requires_approval` 业务语义：仅对外协（OUTSOURCE）有意义；INHOUSE 强制 false（service 层
//! enforce，与 Python `_assert_inhouse_no_approval` 对齐）。

use sqlx::FromRow;

/// `t_process` 行（完整列，phase P2 CRUD 全字段）
#[derive(Debug, Clone, FromRow)]
pub struct TProcess {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub category: String,
    pub sort_order: i32,
    pub description: Option<String>,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
    pub requires_approval: bool,
}