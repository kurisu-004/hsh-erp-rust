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

/// 草稿卡片「最近批次」展示行（`t_part_batch JOIN t_part` 投影）。
///
/// 2026-08-22 新增：配合 `ScanDeliveryNoteSummaryDto::recent_items` 返回。
/// 只投影卡片展示所需的 6 列（batch_id / part_id / serial_no / drawing_no /
/// name / order_no），比 `TPartBatch + TPart` 轻量。
#[derive(Debug, Clone)]
pub struct RecentBatchRow {
    pub batch_id: i64,
    pub part_id: i64,
    /// `t_part.serial_no` 是 nullable（手工工单可没序列号）。
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    /// `t_part.order_no` 是 nullable。
    pub order_no: Option<String>,
}