//! statistics 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为 `overview.rs`（Overview 端点 + 公共原子）/ `worker.rs`
//! （WorkerStats / WorkerDetail 端点）。
//!
//! 2026-10-12：原 `pickup_skip.rs`（PickupSkipSummary / PickupSkipDetail 端点）
//! 随跳序取件域清空一并删除 —— 数据源 `t_pickup_skip_event` 的写入点早已随
//! Python v1 退役消失，本域不再有可统计的口径。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止**出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

// 2026-09-22 PR4：复制自 iam/vo/ 范本。

pub mod overview;
pub mod worker;

pub use overview::{DayCount, DeliveryPerformance, OverviewOut, StatusCount};
pub use worker::{
    WorkerBrief, WorkerDetailOut, WorkerPartItem, WorkerProcessBreakdown, WorkerStatsItem,
    WorkerStatsListOut,
};
