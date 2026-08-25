use serde::Serialize;
use crate::shared::types::serialize_i64;

#[derive(Debug, Clone, Serialize)]
pub struct TakenItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefillResult {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub taken: Vec<TakenItem>,
    pub pool_empty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessPoolCount {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub pool_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerPoolState {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    pub worker_name: String,
    pub work_type_code: String,
    pub max_held: i32,
    pub current_held: i64,
    pub capacity_remaining: i32,
    pub pool_count_by_process: Vec<ProcessPoolCount>,
}