//! dashboard 域 DTO（2026-09-15 takeover-fill）
//!
//! 2026-09-22 PR4：本文件原承载 WS 消息外壳（`WsSnapshotMsg` / `WsEventMsg` /
//! `WsHeartbeatMsg`）和大屏 snapshot 子结构（`DashboardSnapshot` /
//! `OnProductionShelfGroup` / `DashboardItem` / `UpcomingDeliveryBucket`）——
//! 全部为 Serialize 出参，已迁出至 `super::vo`。
//!
//! dashboard 是 WS-only 域，无 HTTP request body 入参（鉴权走 query `WsQuery`
//! 字段 token + JWT 验签，定义在 handler.rs）。本文件留空保留模块位置以避免
//! 破坏既有 `pub mod dto;` 引用；后续如新增 HTTP 入参 DTO，在此文件加。
//!
//! ## 与 `super::vo` 的边界
//! 出参 VO 全部位于 `super::vo`；本文件不承载任何结构体。
//!
//! ## 历史
//! - 2026-09-15 takeover-fill：dashboard dto.rs 初始承载 WS 消息外壳 +
//!   大屏 snapshot 数据结构（service 不持有数据类）。
//! - 2026-09-22 Group E：snapshot 数据结构从 service.rs 平移过来，统一在 dto.rs。
//! - 2026-09-22 PR4：所有出参迁至 `super::vo`，本文件留空。