//! statistics 域 worker 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

use super::overview::DayCount;

#[derive(Debug, Clone, Serialize)]
pub struct WorkerStatsItem {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    pub worker_name: String,
    pub badge_code: String,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub work_type_id: Option<i64>,
    pub work_type_name: Option<String>,
    pub is_active: bool,

    pub pickup_count: i64,
    pub pickup_quantity: i64,
    pub participated_part_count: i64,
    /// 贡献度百分比 0-100；公式见 StatisticsService::_compute_contribution
    pub contribution_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerStatsListOut {
    pub items: Vec<WorkerStatsItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerBrief {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub badge_code: String,
    pub work_type_name: Option<String>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerPartItem {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub status: String,
    pub pickup_count: i64,
    pub last_pickup_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerDetailOut {
    pub worker: WorkerBrief,
    pub pickup_count: i64,
    pub pickup_quantity: i64,
    pub participated_part_count: i64,
    pub return_count: i64,
    pub daily_pickups: Vec<DayCount>,
    pub parts: Vec<WorkerPartItem>,
}