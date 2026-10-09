//! statistics 域 DTO（2026-09-15 takeover-fill）
//!
//! 对应 Python myERP/schema/statistics.py。三个聚合段：
//! - Overview：基础计数 + 图表
//! - WorkerStats：工人贡献度一览
//! - WorkerDetail：单工人详情（持有件 / 完成件数）
//!
//! ## 出参 VO 归属（2026-09-22 PR4）
//! `OverviewOut` / `WorkerStatsListOut` / `WorkerStatsItem` / `WorkerDetailOut` /
//! `WorkerBrief` / `WorkerPartItem` / `DayCount` / `DeliveryPerformance` /
//! `StatusCount` 均已迁出至 `super::vo`。
//!
//! 2026-10-12：原 `PickupSkipSummary` / `PickupSkipDetail` 两段与入参
//! `PickupSkipDetailQuery` 随数据源表 `t_pickup_skip_event` 删除一并清空。
//!
//! 2026-10-12：有效工时维度**不引入任何入参** —— 它由已有的
//! `DateRangeQuery` 区间 + `t_part_event` 事件派生，无独立筛选条件。
//! `WorkerProcessBreakdown` 是**出参**（`super::vo`），不在本文件。
//!
//! 本文件仅保留入参 DTO（axum extractor 反序列化目标）：
//! - `DateRangeQuery` —— overview / workers / workers/{id} 端点 query

use chrono::NaiveDate;
use serde::Deserialize;

// ============================================================
// 入参：日期 query
// ============================================================

#[derive(Debug, Clone, Deserialize)]
pub struct DateRangeQuery {
    pub date_from: NaiveDate,
    pub date_to: NaiveDate,
}
