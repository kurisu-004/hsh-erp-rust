//! work_type 域数据模型
//!
//! 对应 Python myERP/model/work_type.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只投影 delivery_note 后续会用到的列：
//! - `id` / `code` / `name` / `max_held_batches`
//! - 完整列在 work_type 域业务实施阶段扩展（description / sort_order 不在 P1 视野）。

use chrono::NaiveDateTime;

/// `t_work_type` 行（Phase P1 投影）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TWorkType {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub max_held_batches: Option<i32>,
}