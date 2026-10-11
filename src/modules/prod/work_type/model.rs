//! work_type 域数据模型
//!
//! 对应 Python myERP/model/work_type.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P5（work_type 域 CRUD + mapping）扩展为完整列：
//! - `id` / `code` / `name` / `description` / `sort_order`
//! - `max_held_batches`（工种工人最多可同时持有批次数；NULL=未配置 ⇒ 不可取件）
//! - `auto_allocate_limit`（2026-10-15 新增：自动补料上限；NULL=未配置 ⇒
//!   自动路径兜底 `max_held_batches`；有值须 1..=max_held_batches）
//! - 审计字段：version / created_* / updated_* / deleted_at

use chrono::NaiveDateTime;

/// `t_work_type` 行（Phase P5 完整列）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TWorkType {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub sort_order: i32,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub max_held_batches: Option<i32>,
    /// 2026-10-15 新增（migration `20261015000001_001_...`）：自动补料上限，
    /// 语义见列注释与 `docs/api/queue.md` §4。
    pub auto_allocate_limit: Option<i32>,
}
