//! statistics 域 DTO（2026-09-15 takeover-fill）
//!
//! 对应 Python myERP/schema/statistics.py。三个聚合段：
//! - Overview：基础计数 + 图表
//! - WorkerStats：工人贡献度一览
//! - WorkerDetail：单工人详情（持有件 / 完成件数 / 跳序次数）
//! - PickupSkipSummary / PickupSkipDetail：跳序取件汇总 / 明细
//!
//! ## id 序列化约定
//! 雪花 i64 字段用 `serialize_i64` / `serialize_i64_opt`（Global Constraint #3）。

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::shared::types::{serialize_i64, serialize_i64_opt};

// ============================================================
// 图表公共原子
// ============================================================

/// 日期 + 计数（daily_created / daily_completed / daily_pickups 共用）。
#[derive(Debug, Clone, Serialize)]
pub struct DayCount {
    pub date: NaiveDate,
    pub count: i64,
}

/// delivery_performance：基于 delivered_count 集合再细分的命中数。
///
/// - on_time : 实际交期 <= 计划交期
/// - orange  : 实际 > 计划 且 (system IS NULL OR 实际 <= system)
/// - red     : system 存在 且 实际 > system
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryPerformance {
    pub on_time: i64,
    pub orange: i64,
    pub red: i64,
}

/// status_distribution：当前各状态零件数（status_value, count）。
#[derive(Debug, Clone, Serialize)]
pub struct StatusCount {
    pub status_value: String,
    pub count: i64,
}

// ============================================================
// tab1 OverviewOut
// ============================================================

#[derive(Debug, Clone, Serialize)]
pub struct OverviewOut {
    pub date_from: NaiveDate,
    pub date_to: NaiveDate,

    pub created_count: i64,
    /// 期内完成工单数（COMPLETED 事件 batch_id IS NULL，count distinct part_id）
    pub completed_count: i64,
    /// 期末在制：事件重构（任意 COMPLETED/CANCELLED 工单级事件 created_at < date_to+1 → 排除）
    pub in_process_count: i64,
    pub delivered_count: i64,
    /// 期内总产值 sum(total_price)（精确到分，Decimal JSON 自动序列化为 string）
    pub delivered_value: Decimal,
    pub late_orange_count: i64,
    pub late_red_count: i64,
    pub overdue_undelivered_count: i64,
    pub repair_part_count: i64,

    pub daily_created: Vec<DayCount>,
    pub daily_completed: Vec<DayCount>,
    pub delivery_performance: DeliveryPerformance,
    pub status_distribution: Vec<StatusCount>,
}

// ============================================================
// tab2 WorkerStatsListOut
// ============================================================

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

// ============================================================
// tab3 WorkerDetailOut
// ============================================================

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

// ============================================================
// tab4 跳序取件
// ============================================================

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

// ============================================================
// 入参：日期 query
// ============================================================

#[derive(Debug, Clone, Deserialize)]
pub struct DateRangeQuery {
    pub date_from: NaiveDate,
    pub date_to: NaiveDate,
}

/// `GET /api/v2/statistics/pickup-skips/{worker_id}` 入参：
/// `worker_id` 路径（雪花 ID 字符串）+ `limit`/`offset` query。
#[derive(Debug, Clone, Deserialize)]
pub struct PickupSkipDetailQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}