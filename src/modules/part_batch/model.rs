//! part_batch 域数据模型
//!
//! 对应 Python myERP/model/part_batch.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只投影 delivery_note / delivery_group 后续会用到的列：
//! 标识 + 工单 + 批次号 + 数量 + 状态 + 位置 + holder + next_process + placed_at +
//! 送货单关联 + 父批次 + 返修标 + 乐观锁 + 软删。
//! Python `TPartBatch` 的其他字段留到 part_batch 域实施阶段扩展。

use chrono::{NaiveDate, NaiveDateTime};

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

/// part-batches 端点的批次窄字段中间结构（service 内短暂使用，DTO 转换见 service）。
/// SQL 列别名见 repo `list_active_by_part_id_with_holder`。
#[derive(Debug, Clone)]
pub struct PartBatchScanRow {
    pub id: i64,
    pub quantity: i32,
    pub status: String,
    pub holder_name: Option<String>,
    pub version: i32,
}

// ===== Inspection Batch List =====

/// `GET /parts/inspection-batches` 单行中间结构（repo ↔ service 边界类型）。
///
/// SQL 列别名见 repo `list_batches_with_part`（单次 JOIN 8 表，含 holder_name
/// / next_process_name / delivery_note_no / customer_name / l1_customer_name
/// 全部解析）。
#[derive(Debug, Clone)]
pub struct InspectionBatchListRow {
    // 批次
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String,
    pub location: Option<String>,
    pub version: i32,
    pub placed_at: Option<NaiveDateTime>,
    pub has_been_repaired: bool,
    pub parent_batch_id: Option<i64>,
    // holder / process / delivery_note 解析
    pub current_holder_id: Option<i64>,
    pub holder_name: Option<String>,
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,
    pub delivery_note_id: Option<i64>,
    pub delivery_note_no: Option<String>,
    // 工单
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub order_no: Option<String>,
    pub planned_delivery_date: NaiveDate,
    pub is_urgent: bool,
    pub part_version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    // 客户
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

impl From<InspectionBatchListRow>
    for crate::modules::part::dto::InspectionBatchListItemOut
{
    fn from(r: InspectionBatchListRow) -> Self {
        Self {
            batch_id: r.batch_id,
            batch_no: r.batch_no,
            quantity: r.quantity,
            status: r.status,
            location: r.location,
            version: r.version,
            placed_at: r.placed_at,
            has_been_repaired: r.has_been_repaired,
            parent_batch_id: r.parent_batch_id,
            current_holder_id: r.current_holder_id,
            holder_name: r.holder_name,
            next_process_id: r.next_process_id,
            next_process_name: r.next_process_name,
            delivery_note_id: r.delivery_note_id,
            delivery_note_no: r.delivery_note_no,
            part_id: r.part_id,
            serial_no: r.serial_no,
            drawing_no: r.drawing_no,
            name: r.name,
            order_no: r.order_no,
            planned_delivery_date: r.planned_delivery_date,
            is_urgent: r.is_urgent,
            part_version: r.part_version,
            created_at: r.created_at,
            updated_at: r.updated_at,
            customer_id: r.customer_id,
            customer_name: r.customer_name,
            l1_customer_name: r.l1_customer_name,
        }
    }
}