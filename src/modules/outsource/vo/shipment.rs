//! outsource 域 shipment / in_flight / approved_for_send 端点响应 VO
//! （2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// 外协发货记录详情。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceShipmentOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub version: i32,
    #[serde(serialize_with = "serialize_i64")]
    pub quote_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub batch_id: Option<i64>,
    pub batch_no: Option<i32>,
    #[serde(serialize_with = "serialize_i64")]
    pub outsource_company_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub quantity: i32,
    pub unit_price: String,
    pub status: String,
    pub sent_at: NaiveDateTime,
    pub received_at: Option<NaiveDateTime>,
    pub is_billed: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    pub outsource_company_name: Option<String>,
    pub process_name: Option<String>,
    pub customer_path: Option<String>,
}

/// 外协中批次列表（in_flight）。
///
/// 2026-10-03 接上线（`GET /outsource-shipments/in-flight`，替代 part 域错形状的
/// `/parts/outsource-in-flight`）。驱动 SQL 改为 INNER JOIN 主导：
/// `t_outsource_shipment` ⋈ `t_part_batch` ⋈ `t_part`，故 6 个字段的
/// `Option` 收成必填。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceInFlightItem {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// `t_part_batch.id`（部分接收端点的路径锚点）。
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    /// **`t_part_batch.quantity`（当前剩余待收量）**，不是 `shipment.quantity`
    /// ——前端拿它做部分接收的 max 值。
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub drawing_no: Option<String>,
    pub name: Option<String>,
    pub is_urgent: bool,
    pub customer_path: Option<String>,
    /// 外协加工的工序（= `t_outsource_shipment.process_id`）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,
    #[serde(serialize_with = "serialize_i64")]
    pub outsource_company_id: i64,
    pub outsource_company_name: Option<String>,
    pub sent_at: NaiveDateTime,
    /// **`t_part_batch.version`**（不是 `shipment.version`）——前端拿它当
    /// `receive-from-outsource` 的 OCC 锚。
    pub version: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutsourceInFlightListOut {
    pub items: Vec<OutsourceInFlightItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 外协对账页单行：某公司已发出的一个零件。
///
/// 2026-10-03 新增（`GET /outsource-companies/{id}/sent-parts`）。**刻意不复用
/// `OutsourceShipmentOut`**：后者是 reconcile-update 写端点的出参，主键字段叫
/// `id`；本 VO 主键叫 `shipment_id`（前端行编辑端点入参按此名取）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceSentPartOut {
    /// `t_outsource_shipment.id`。
    #[serde(serialize_with = "serialize_i64")]
    pub shipment_id: i64,
    /// shipment 行 OCC（reconcile-update 必传）。
    pub version: i32,
    #[serde(serialize_with = "serialize_i64")]
    pub quote_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    /// 客户路径：有 L1 拼 `L1 / L2`，否则仅 L2 名，缺客户为 `null`。
    pub customer_path: Option<String>,
    /// 历史行可能为 `null`（shipment 未绑批次）。
    pub batch_no: Option<i32>,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_name: Option<String>,
    pub quantity: i32,
    /// Decimal 字符串。
    pub unit_price: String,
    /// `unit_price × quantity`，Decimal 字符串。
    pub total_price: String,
    pub sent_at: NaiveDateTime,
    pub received_at: Option<NaiveDateTime>,
    /// `"OUTSOURCING"` / `"RECEIVED"`。
    pub status: String,
    pub is_billed: bool,
    pub is_urgent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutsourceSentPartListOut {
    pub items: Vec<OutsourceSentPartOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// 2026-10-03 删除 `ApprovedForSendItem` / `ApprovedForSendListOut`（死 VO，零调用方）。
// 它们想表达的是「可发送外协」，但形状是「必须先有 APPROVED 报价」—— 表达不了
// DIRECT 模式（无报价直发），也没有 `company_options` / `send_mode` / `quote_id`。
// 取代者：`super::sendable::OutsourceSendableItem`（`GET /outsource-sendable`），
// 同一行同时覆盖 APPROVAL 与 DIRECT 两种模式。
