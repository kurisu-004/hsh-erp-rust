//! outsource 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按读模型拆为 `company.rs`（Company 端点）/ `quote.rs`（Quote lifecycle 出参）/
//! `shipment.rs`（Shipment 出参 + in-flight / sent-parts 两个列表）/
//! `quotable.rs`（`quotable-parts` 读模型）/ `sendable.rs`（`/outsource-sendable`
//! 读模型）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止**出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。
//! （`sendable::OutsourceCompanyOption` 是唯一例外：它同时是 repo 层
//! `array_agg` JSON 的解码目标，故双派生 `Serialize + Deserialize`。）

// 2026-09-22 PR4：复制自 iam/vo/ 范本。

pub mod company;
pub mod quotable;
pub mod quote;
pub mod sendable;
pub mod shipment;

pub use company::{
    OutsourceCompanyListOut, OutsourceCompanyOut, OutsourceCompanyProcessLinkOut,
    OutsourceCompanyWithProcessesOut,
};
pub use quotable::{QuotablePartListOut, QuotablePartOut};
pub use quote::{OutsourceQuoteListOut, OutsourceQuoteOut};
pub use sendable::{OutsourceCompanyOption, OutsourceSendableItem, OutsourceSendableListOut};
pub use shipment::{
    OutsourceInFlightItem, OutsourceInFlightListOut, OutsourceSentPartListOut,
    OutsourceSentPartOut, OutsourceShipmentOut,
};
