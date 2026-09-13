//! outsource 域 DTO（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/schema/outsource.py。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxOut`：单条详情出参（id 字段用 `#[serde(serialize_with = shared::types::serialize_i64)]`）
//! - `XxxListItem` / `XxxListOut`：列表分页
//! - `XxxListQuery`：列表查询参数

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::shared::types::{serialize_i64, serialize_i64_opt};

// ===========================================================================
// 出参 — Company
// ===========================================================================

/// 外协公司详情（不含工序映射）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub address: Option<String>,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 外协公司详情（含工序映射）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyWithProcessesOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub address: Option<String>,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub processes: Vec<OutsourceCompanyProcessLinkOut>,
}

/// 公司 ↔ 工序 映射出参。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyProcessLinkOut {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub process_name: String,
    pub category: String,
    pub sort_order: i32,
}

/// 外协公司列表出参。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyListOut {
    pub items: Vec<OutsourceCompanyOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ===========================================================================
// 入参 — Company
// ===========================================================================

/// 创建外协公司（可选一并写入工序能力清单）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceCompanyCreateRequest {
    pub name: String,
    #[serde(default)]
    pub contact_name: Option<String>,
    #[serde(default)]
    pub contact_phone: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default = "default_is_active")]
    pub is_active: bool,
    /// 可选：创建时一并写入工序能力清单（OUTSOURCE 类别的 process_id 列表）。
    #[serde(default)]
    pub process_ids: Option<Vec<String>>,
}

fn default_is_active() -> bool {
    true
}

/// 更新外协公司（字段可选 + 显式 OCC）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceCompanyUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub contact_name: Option<Option<String>>,
    #[serde(default)]
    pub contact_phone: Option<Option<String>>,
    #[serde(default)]
    pub address: Option<Option<String>>,
    #[serde(default)]
    pub is_active: Option<bool>,
    pub version: i32,
}

/// 整体替换工序能力清单。
#[derive(Debug, Clone, Deserialize)]
pub struct SetOutsourceCompanyProcessRequest {
    pub process_ids: Vec<String>,
}

/// 公司列表查询参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceCompanyListQuery {
    #[serde(default)]
    pub name_like: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ===========================================================================
// 出参 — Quote
// ===========================================================================

/// 外协报价详情出参（含 part / company / process / customer 名称补全）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQuoteOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub version: i32,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub outsource_company_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub price: String,
    pub note: Option<String>,
    pub status: String,
    pub submitted_at: Option<NaiveDateTime>,
    pub reviewed_at: Option<NaiveDateTime>,
    pub review_note: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    // 展示用补全字段（service 拼装）
    pub part_serial_no: Option<String>,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    pub outsource_company_name: Option<String>,
    pub process_code: Option<String>,
    pub process_name: Option<String>,
    pub customer_path: Option<String>,
    pub part_unit_price: Option<String>,
    pub is_urgent: bool,
}

/// 报价列表出参。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQuoteListOut {
    pub items: Vec<OutsourceQuoteOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ===========================================================================
// 入参 — Quote
// ===========================================================================

/// 创建 DRAFT 报价。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteCreateRequest {
    pub part_id: String,
    pub outsource_company_id: String,
    pub process_id: String,
    pub price: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// 更新 DRAFT 报价。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteUpdateRequest {
    #[serde(default)]
    pub price: Option<String>,
    #[serde(default)]
    pub note: Option<Option<String>>,
    pub version: i32,
}

/// 审批通过（MANAGER-only）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteApproveRequest {
    #[serde(default)]
    pub review_note: Option<String>,
    pub version: i32,
}

/// 审批拒绝（MANAGER-only）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteRejectRequest {
    pub review_note: String,
    pub version: i32,
}

/// 报价列表查询参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceQuoteListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub part_id: Option<String>,
    #[serde(default)]
    pub outsource_company_id: Option<String>,
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ===========================================================================
// 出入参 — Shipment
// ===========================================================================

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

/// 对账页更新 shipment。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceShipmentReconcileUpdateRequest {
    #[serde(default)]
    pub unit_price: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub is_billed: Option<bool>,
    pub version: i32,
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

/// 已批准可发送的零件列表查询参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ApprovedForSendListQuery {
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}
