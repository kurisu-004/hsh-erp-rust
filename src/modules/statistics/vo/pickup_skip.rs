//! statistics 域 pickup_skip 端点响应 VO（2026-09-22 PR4 重构）

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::shared::types::serialize_i64;

#[derive(Debug, Clone, Serialize)]
pub struct PickupSkipSummaryItem {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    pub worker_name: String,
    pub badge_code: String,
    pub work_type_name: Option<String>,
    pub skip_count: i64,
    pub last_skip_at: Option<NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PickupSkipSummaryOut {
    pub items: Vec<PickupSkipSummaryItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PickupSkipDetailItem {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub part_name: String,
    pub batch_no: i32,
    pub quantity: i32,
    pub part_planned_delivery_date: Option<NaiveDate>,
    pub skipped_earliest_date: Option<NaiveDate>,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct PickupSkipDetailOut {
    pub items: Vec<PickupSkipDetailItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}