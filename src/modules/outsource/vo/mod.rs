//! outsource 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按读模型拆为 `company.rs`（Company 端点）/ `quote.rs`（Quote lifecycle 出参）/
//! `shipment.rs`（Shipment 出参 + in-flight / sent-parts 两个列表）/
//! `quotable.rs`（`quotable-parts` 读模型）/ `sendable.rs`（`/outsource-sendable`
//! 读模型）/ `pool.rs`（`/outsource-pool/*` 三端点读模型，2026-10-03 新增）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止**出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。
//! （`sendable::OutsourceCompanyOption` 是唯一例外：它同时是 repo 层
//! `array_agg` JSON 的解码目标，故双派生 `Serialize + Deserialize`；`pool.rs`
//! 内嵌复用同一类型，不再各自定义。）
//!
//! ## 2026-10-03 新增 `pool.rs`
//! `GET /outsource-pool/*` 是看板三件套（counts / {process_id} / state），形态
//! 照抄 `prod::pool`。候选侧与 `sendable.rs` 同源 SQL，但**不复用
//! `OutsourceSendableItem`**：看板视角工序已提到顶层，带 `next_process_*` 会
//! 出现两个真相源。

// 2026-09-22 PR4：复制自 iam/vo/ 范本。

pub mod company;
pub mod pool;
pub mod quotable;
pub mod quote;
pub mod sendable;
pub mod shipment;

pub use company::{
    OutsourceCompanyListOut, OutsourceCompanyOut, OutsourceCompanyProcessLinkOut,
    OutsourceCompanyWithProcessesOut,
};
pub use pool::{
    OutsourceHeldBatchItem, OutsourcePoolCandidate, OutsourcePoolCompanyOut,
    OutsourcePoolCountsOut, OutsourcePoolDetailOut, OutsourcePoolProcessCount,
    OutsourcePoolStateOut,
};
pub use quotable::{QuotablePartListOut, QuotablePartOut};
pub use quote::{OutsourceQuoteListOut, OutsourceQuoteOut};
pub use sendable::{OutsourceCompanyOption, OutsourceSendableItem, OutsourceSendableListOut};
pub use shipment::{
    OutsourceInFlightItem, OutsourceInFlightListOut, OutsourceSentPartListOut,
    OutsourceSentPartOut, OutsourceShipmentOut,
};
