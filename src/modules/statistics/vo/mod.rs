//! statistics 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为 `overview.rs`（Overview 端点 + 公共原子）/ `worker.rs`
//! （WorkerStats / WorkerDetail 端点）/ `pickup_skip.rs`（PickupSkipSummary /
//! PickupSkipDetail 端点）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

// 2026-09-22 PR4：复制自 iam/vo/ 范本。

pub mod overview;
pub mod pickup_skip;
pub mod worker;

pub use overview::{DayCount, DeliveryPerformance, OverviewOut, StatusCount};
pub use pickup_skip::{PickupSkipDetailItem, PickupSkipDetailOut, PickupSkipSummaryItem, PickupSkipSummaryOut};
pub use worker::{WorkerBrief, WorkerDetailOut, WorkerPartItem, WorkerStatsItem, WorkerStatsListOut};