//! statistics 域 overview 端点响应 VO（2026-09-22 PR4 重构）
//!
//! 含公共原子 `DayCount` / `DeliveryPerformance` / `StatusCount` —— 被
//! 其他端点（worker_daily_pickups）也复用。

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Serialize;

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