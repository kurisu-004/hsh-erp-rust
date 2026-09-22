//! part 域 inspection 端点出参 VO（2026-09-22 PR4 重构）

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::modules::part::batch::model::InspectionBatchListRow;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `GET /parts/inspection-batches` 列表行：批次 + 工单 + 客户 + holder/process/
/// delivery_note 名称（一次性 JOIN 解析，不在 service 做 N+1）。
///
/// 字段命名沿用 v1 `PartOut`/`PartBatchOut` 约定（`batch_id` 即 `t_part_batch.id`，
/// `version` 即乐观锁版本号）。前端用 `batch_id + version` 直接拼
/// `POST /parts/{part_id}/to-ship` 或 `to-inspection` 的请求体。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `has_been_repaired` 字段
/// （t_part_batch 列已删；返修事实由 t_part_event REPAIR_STARTED 事件追溯）。
///
/// 2026-09-16 PR-3 批次 step 化（migration 028）：
/// - 删 `placed_at`（t_part_batch 列已删，不再统计生产时间）
/// - 新增 `current_process_step_id`：逻辑 FK → t_process_chain_step.id
///   （批次当前所处的工艺链步骤；NULL = 批次尚未进入生产流或 part 无链）
/// - `next_process_id` / `next_process_name` 字段保留，由 repo JOIN step 派生
///   （保持 DTO 兼容，不破坏前端）
#[derive(Debug, Clone, Serialize)]
pub struct InspectionBatchListItemOut {
    // ===== 批次字段 =====
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String, // 必为 "INSPECTION"
    pub location: Option<String>,
    pub version: i32,
    /// 逻辑 FK → t_process_chain_step.id（2026-09-16 PR-3；替代 next_process_id 列）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_process_step_id: Option<i64>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub parent_batch_id: Option<i64>,

    // ===== holder 解析（COALESCE 三表）=====
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_holder_id: Option<i64>,
    pub holder_name: Option<String>,
    /// 派生自 current_process_step_id（JOIN step.process_id）；保留字段名以
    /// 兼容前端契约（2026-09-16 PR-3）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,

    // ===== delivery_note 解析 =====
    #[serde(serialize_with = "serialize_i64_opt")]
    pub delivery_note_id: Option<i64>,
    pub delivery_note_no: Option<String>,

    // ===== 工单字段（JOIN t_part）=====
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub order_no: Option<String>,
    pub planned_delivery_date: NaiveDate,
    pub is_urgent: bool,
    pub part_version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,

    // ===== 客户解析（JOIN t_customer + 自连 L1）=====
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

impl From<InspectionBatchListRow> for InspectionBatchListItemOut {
    fn from(r: InspectionBatchListRow) -> Self {
        Self {
            batch_id: r.batch_id,
            batch_no: r.batch_no,
            quantity: r.quantity,
            status: r.status,
            location: r.location,
            version: r.version,
            current_process_step_id: r.current_process_step_id,
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

/// `GET /parts/inspection-batches` 出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionBatchListOut {
    pub items: Vec<InspectionBatchListItemOut>,
    #[serde(serialize_with = "serialize_i64")]
    pub total: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub limit: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub offset: i64,
}

/// `GET /parts/repair-batches` / `repairing-batches` 出参：返修批次列表（复用 InspectionBatchListOut）。
pub type RepairBatchesOut = InspectionBatchListOut;