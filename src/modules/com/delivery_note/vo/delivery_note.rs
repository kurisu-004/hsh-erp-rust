//! delivery_note 域 P2 送货单 CRUD 端点响应 VO

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

/// 送货单概要（list + 大部分接口的公共响应）。
///
/// 2026-10-08：删掉 4 个范围字段（`delivery_group_id` / `delivery_group_name` /
/// `leaf_customer_id` / `leaf_customer_name`）与 `scope_label`。范围三态判定已下线、
/// 建单判定键收敛为 `(customer_id, DRAFT)` 单键 ⇒ 「这张单属于哪个分组 / 哪个单厂」
/// 不再是单据属性，展示口径统一退化为「L1 客户名 + `customer_path`」。
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
    /// 2026-10-04 新增：装配件工单总套数（`t_assembly.quantity`）。
    ///
    /// 仅子件行填；散件为 `None`。`Option<i32>` 走普通 serde（不需要
    /// `serialize_i64_opt` —— 那套是给 > 2^53 的雪花 id 用的）。
    pub assembly_quantity: Option<i32>,
    /// 2026-10-04 新增：本单可出货套数（**只统计本单**批次，口径见
    /// `service::shippable_sets::note_shippable_sets`）。
    ///
    /// `min` 的定义域是「该装配件的**全部**子件」：本单没交批次的子件以 0 参与
    /// ⇒ 凑不齐整套就是 0。须与打印注入的 `merge_quantities` 逐字同值（同一纯
    /// 函数、同一子件集），否则前端预览与导出 xlsx 会给出两个数。
    ///
    /// 仅子件行填；散件为 `None`。0 = 凑不齐整套（打印时不进 xlsx）。
    /// 装配件被软删 / 不存在时同样是 `None`（与 `assembly_id` 同口径：都取
    /// 「解析到的装配件」）。
    pub shippable_sets: Option<i32>,
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

/// `GET /api/v2/com/delivery/note/batch-detail?ids=...` 响应载体。
///
/// 仅作为 `items: [DeliveryNoteDetailOut]` 的轻量封装，避免 schema 顶层直接
/// 给出数组（信封 `data` 不能是裸数组）。`DeliveryNoteDetailOut` 自身已
/// `#[serde(flatten)] head: DeliveryNoteOut`，因此每个 item 在 wire 上仍是
/// head + `line_items` + `scanned_serials` 的扁平结构。
#[derive(Debug, Clone, Serialize)]
pub struct BatchDeliveryDetailData {
    pub items: Vec<DeliveryNoteDetailOut>,
}

/// 一览响应（GET /api/v2/com/delivery/note；含分页总计）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteListOut {
    pub items: Vec<DeliveryNoteOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
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
