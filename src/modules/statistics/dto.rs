//! statistics 域 DTO（2026-09-15 takeover-fill）
//!
//! 对应 Python myERP/schema/statistics.py。三个聚合段：
//! - Overview：基础计数 + 图表
//! - WorkerStats：工人贡献度一览
//! - WorkerDetail：单工人详情（持有件 / 完成件数 / 跳序次数）
//! - PickupSkipSummary / PickupSkipDetail：跳序取件汇总 / 明细
//!
//! ## 出参 VO 归属（2026-09-22 PR4）
//! `OverviewOut` / `WorkerStatsListOut` / `WorkerStatsItem` / `WorkerDetailOut` /
//! `WorkerBrief` / `WorkerPartItem` / `PickupSkipSummaryOut` / `PickupSkipSummaryItem` /
//! `PickupSkipDetailOut` / `PickupSkipDetailItem` / `DayCount` /
//! `DeliveryPerformance` / `StatusCount` 均已迁出至 `super::vo`。
//!
//! 本文件仅保留入参 DTO（axum extractor 反序列化目标）：
//! - `DateRangeQuery` —— overview / workers / workers/{id} 端点 query
//! - `PickupSkipDetailQuery` —— pickup-skips/{worker_id} 端点 query

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

/// `GET /api/v2/statistics/pickup-skips/{worker_id}` 入参：
/// `worker_id` 路径（雪花 ID 字符串）+ `limit`/`offset` query。
#[derive(Debug, Clone, Deserialize)]
pub struct PickupSkipDetailQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}