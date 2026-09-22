//! outsource 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为 `company.rs`（Company 端点）/ `quote.rs`（Quote 端点）/
//! `shipment.rs`（Shipment / InFlight / ApprovedForSend 端点）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

// 2026-09-22 PR4：复制自 iam/vo/ 范本。

pub mod company;
pub mod quote;
pub mod shipment;

pub use company::{OutsourceCompanyListOut, OutsourceCompanyOut, OutsourceCompanyProcessLinkOut, OutsourceCompanyWithProcessesOut};
pub use quote::{OutsourceQuoteListOut, OutsourceQuoteOut};
pub use shipment::{
    ApprovedForSendItem, ApprovedForSendListOut, OutsourceInFlightItem, OutsourceInFlightListOut,
    OutsourceShipmentOut,
};