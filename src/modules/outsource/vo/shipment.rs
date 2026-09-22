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
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceInFlightItem {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub batch_id: Option<i64>,
    pub batch_no: Option<i32>,
    pub quantity: Option<i32>,
    pub serial_no: Option<String>,
    pub drawing_no: Option<String>,
    pub name: Option<String>,
    pub is_urgent: bool,
    pub customer_path: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub outsource_company_id: Option<i64>,
    pub outsource_company_name: Option<String>,
    pub sent_at: Option<NaiveDateTime>,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutsourceInFlightListOut {
    pub items: Vec<OutsourceInFlightItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 已批准可发送的零件列表（含 APPROVED quote + 可发送 part）。
#[derive(Debug, Clone, Serialize)]
pub struct ApprovedForSendItem {
    pub version: i32,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub part_serial_no: Option<String>,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    pub quantity: i32,
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    pub batch_quantity: i32,
    pub planned_delivery_date: Option<String>,
    pub is_urgent: bool,
    pub customer_path: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,
    pub shelf_code: Option<String>,
    #[serde(serialize_with = "serialize_i64")]
    pub outsource_company_id: i64,
    pub outsource_company_name: Option<String>,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_name: Option<String>,
    pub price: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApprovedForSendListOut {
    pub items: Vec<ApprovedForSendItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}