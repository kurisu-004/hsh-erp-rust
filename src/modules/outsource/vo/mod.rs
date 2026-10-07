// 2026-09-22 PR4：复制自 iam/vo/ 范本。
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按读模型拆为 `company.rs`（Company 端点）/ `quote.rs`（Quote lifecycle 出参）/
//! `shipment.rs`（Shipment 出参 + in-flight / sent-parts 两个列表）/
//! `quotable.rs`（`quotable-parts` 读模型）/ `sendable.rs`（`/outsource-sendable`
//! 读模型）/ `queue.rs`（`/outsource-queue/*` 看板两读的出参，2026-10-09 新增，
//! 取代原 `pool.rs`）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止**出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。
//! （`sendable::OutsourceCompanyOption` 是唯一例外：它同时是 repo 层
//! `array_agg` JSON 的解码目标，故双派生 `Serialize + Deserialize`；
//! `queue.rs` 的候选卡复用同一类型，不各自定义。）
//!
//! ## 2026-10-09：`pool.rs` → `queue.rs`
//! `GET /outsource-pool/{counts,state,{process_id}}` 三条旧读被
//! `GET /outsource-queue/snapshot` + `GET /outsource-queue/processes/{id}` 取代
//! （硬切无 alias）。字段取舍（候选卡 3 删 1 拆 5 加、在途卡加
//! `has_cnc_program`）与内联 held 批次的理由见 `queue.rs` 文件头。

pub mod company;
pub mod queue;
pub mod quotable;
pub mod quote;
pub mod sendable;
pub mod shipment;

pub use company::{
    OutsourceCompanyListOut, OutsourceCompanyOut, OutsourceCompanyProcessLinkOut,
    OutsourceCompanyWithProcessesOut,
};
pub use queue::{
    OutsourceQueueCandidate, OutsourceQueueCompany, OutsourceQueueHeldBatch, OutsourceQueueProcess,
    OutsourceQueueProcessDetail, OutsourceQueueProcessMeta, OutsourceQueueSnapshot,
};
pub use quotable::{QuotablePartListOut, QuotablePartOut};
pub use quote::{OutsourceQuoteListOut, OutsourceQuoteOut};
pub use sendable::{OutsourceCompanyOption, OutsourceSendableItem, OutsourceSendableListOut};
pub use shipment::{
    OutsourceInFlightItem, OutsourceInFlightListOut, OutsourceSentPartListOut,
    OutsourceSentPartOut, OutsourceShipmentOut,
};
