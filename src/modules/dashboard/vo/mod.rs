//! dashboard 域响应 VO（HTTP / WS 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；dashboard 无 HTTP 入参 DTO（WS upgrade 走
//! `WsQuery` query 参数 + JWT 鉴权，见 handler.rs）。
//! 按职责拆为 `snapshot.rs`（大屏 snapshot 子结构 + WS 消息外壳）。
//!
//! ## 与 dto 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `serde_json::to_string(&envelope)` 序列化。
//!
//! ## 2026-09-22 PR4 整理
//! dashboard/dto.rs 原同时承担 WS 消息外壳（`WsSnapshotMsg` / `WsEventMsg` /
//! `WsHeartbeatMsg`）和大屏 snapshot 子结构（`DashboardSnapshot` /
//! `OnProductionShelfGroup` / `DashboardItem` / `UpcomingDeliveryBucket`）——
//! 全部出参均迁移到 vo/snapshot.rs。`dashboard/dto.rs` 留空（dashboard 无
//! Deserialize-only DTO），handler 仍可 import `WsQuery`（位于 handler.rs）。
//!
//! 见 `src/modules/dashboard/dto.rs`（保留仅为占位/历史参照）。

// 2026-09-22 PR4：复制自 iam/vo/ 范本。

pub mod snapshot;

pub use snapshot::{
    DashboardItem, DashboardSnapshot, OnProductionShelfGroup, UpcomingDeliveryBucket,
    WsEventMsg, WsHeartbeatMsg, WsSnapshotMsg,
};