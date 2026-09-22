//! delivery_note 域 P2 送货单 CRUD 端点响应 VO

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

/// 送货单概要（list + 大部分接口的公共响应）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub delivery_note_no: String,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub customer_path: Option<String>,
    pub status: String,
    pub submitted_at: Option<NaiveDateTime>,
    pub picked_up_at: Option<NaiveDateTime>,
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub submitted_by: Option<i64>,
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub picked_up_by: Option<i64>,
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub driver_worker_id: Option<i64>,
    pub driver_worker_name: Option<String>,
    pub part_count: i64,
    pub note: Option<String>,
    pub delivery_date: Option<NaiveDate>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    /// 范围字段（D1 范围列）
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub delivery_group_id: Option<i64>,
    pub delivery_group_name: Option<String>,
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub leaf_customer_id: Option<i64>,
    pub leaf_customer_name: Option<String>,
    /// 范围展示文案（设计 §6.2：分组名 / L2 名 / L1 名）
    pub scope_label: Option<String>,
}

/// 送货单下一行零件的投影（行=批次；id = batch_id）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteLineItem {
    /// 批次 id（行身份）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    /// 工单 id
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub batch_label: String,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub status: String,
    pub applicant_name: Option<String>,
    pub request_date: Option<NaiveDate>,
    pub planned_delivery_date: Option<NaiveDate>,
    pub system_delivery_date: Option<NaiveDate>,
    pub order_no: Option<String>,
    pub note: Option<String>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub customer_path: Option<String>,
    /// 兼容字段（前端两种命名都接受）
    pub is_scanned: bool,
    pub scanned: bool,
    /// 装配件父行字段（仅子件行填；散件 None）
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub assembly_id: Option<i64>,
    pub assembly_serial_no: Option<String>,
    pub assembly_drawing_no: Option<String>,
    pub assembly_name: Option<String>,
    pub assembly_order_no: Option<String>,
}

/// 送货单详情（head + line_items + 扫码进度）。
///
/// `scanned_serials` 在 P2 阶段始终为空数组（Python 2026-07-23 起后端不再维护
/// 扫码状态，由前端本地 Set 驱动）；保留字段以保持 schema 兼容。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteDetailOut {
    #[serde(flatten)]
    pub head: DeliveryNoteOut,
    pub line_items: Vec<DeliveryNoteLineItem>,
    pub scanned_serials: Vec<String>,
}

/// `GET /delivery-notes/batch-detail?ids=...` 响应载体。
///
/// 仅作为 `items: [DeliveryNoteDetailOut]` 的轻量封装，避免 schema 顶层直接
/// 给出数组（信封 `data` 不能是裸数组）。`DeliveryNoteDetailOut` 自身已
/// `#[serde(flatten)] head: DeliveryNoteOut`，因此每个 item 在 wire 上仍是
/// head + `line_items` + `scanned_serials` 的扁平结构。
#[derive(Debug, Clone, Serialize)]
pub struct BatchDeliveryDetailData {
    pub items: Vec<DeliveryNoteDetailOut>,
}

/// 送货单事件条目（时间线）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteEventOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub delivery_note_id: i64,
    pub event_type: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub note: Option<String>,
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_by: Option<i64>,
    pub created_at: Option<NaiveDateTime>,
}

/// 扫码响应（P2 始终 `scanned_count=0 / scanned_serials=[] / ready=false`，
/// 与 Python 2026-07-23 起后端行为一致）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNotePickupScanOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub delivery_note_id: i64,
    pub scanned_count: i64,
    pub expected_count: i64,
    pub ready: bool,
    pub scanned_serials: Vec<String>,
}

/// 一览响应（GET /delivery-notes；含分页总计）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteListOut {
    pub items: Vec<DeliveryNoteOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 待司机领取一览（GET /delivery-notes/pickup-pending）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNotePickupListOut {
    pub items: Vec<DeliveryNoteOut>,
}

/// 候选入单零件（INSPECTION + READY_TO_SHIP 批次，同 L1 根，不在 active 单上）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteCandidatePart {
    /// 工单 id（展示 / 反查用）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    /// 批次 id（入单回传用）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    pub batch_label: String,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
    pub quantity: i32,
    pub applicant_name: Option<String>,
    pub status: String,
    pub planned_delivery_date: Option<NaiveDate>,
    pub order_no: Option<String>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub customer_path: Option<String>,
}

/// 候选入单响应（GET /delivery-notes/candidate-parts）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteCandidatePartsOut {
    pub items: Vec<DeliveryNoteCandidatePart>,
}