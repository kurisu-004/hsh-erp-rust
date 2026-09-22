//! outsource 域 quote 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::serialize_i64;

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