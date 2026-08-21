//! part_batch 域数据模型
//!
//! 对应 Python myERP/model/part_batch.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只投影 delivery_note / delivery_group 后续会用到的列：
//! 标识 + 工单 + 批次号 + 数量 + 状态 + 位置 + holder + next_process + placed_at +
//! 送货单关联 + 父批次 + 返修标 + 乐观锁 + 软删。
//! Python `TPartBatch` 的其他字段留到 part_batch 域实施阶段扩展。

use chrono::NaiveDateTime;

/// `t_part_batch` 行（Phase P1 投影）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPartBatch {
    pub id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String,
    pub location: Option<String>,
    pub current_holder_id: Option<i64>,
    pub next_process_id: Option<i64>,
    pub placed_at: Option<NaiveDateTime>,
    pub delivery_note_id: Option<i64>,
    pub parent_batch_id: Option<i64>,
    pub has_been_repaired: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}