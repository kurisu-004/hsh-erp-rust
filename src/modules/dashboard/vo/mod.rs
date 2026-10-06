//! dashboard 域响应 VO（HTTP / WS 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参 DTO 见 `dto.rs`（`DeliveryBasis` 枚举）。
//! 按职责拆为 `snapshot.rs`（大屏 snapshot 子结构 + WS 消息外壳）/
//! `delivery.rs`（交期工单面板 + 柱状图下钻抽屉）/ `upcoming.rs`（交期分桶响应）。
//!
//! ## 与 dto 的边界
//! VO **禁止**出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `serde_json::to_string(&envelope)` 序列化。
//!
//! ## 出参 / 入参侧放置约定
//! 入参结构体（`WsQuery` / `UpcomingQuery` / `DeliveryOrdersQuery`）定义在 handler.rs。

pub mod delivery;
pub mod snapshot;
pub mod upcoming;

pub use delivery::{
    DeliveryOrderDetail, DeliveryOrderDetailOut, SystemDeliveryOrder, SystemDeliveryOrders,
};
pub use snapshot::{
    DashboardSnapshot, UpcomingDeliveryBucket, WorkerHeldBatch, WsEventMsg, WsHeartbeatMsg,
    WsSnapshotMsg,
};
pub use upcoming::UpcomingDeliveryBuckets;
