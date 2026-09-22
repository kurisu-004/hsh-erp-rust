//! delivery_note 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为：
//! - `delivery_group.rs`（P1 送货分组：DeliveryGroup* / UngroupedCustomerOut）
//! - `delivery_note.rs`（P2 送货单 CRUD：DeliveryNoteOut / *ListOut / LineItem / DetailOut / EventOut / PickupScanOut / CandidatePart*）
//! - `attach.rs`（P3 attach-batches：AttachBatchesOut / AttachBatchConflict）
//! - `scan.rs`（P3 扫码：Scan* / BatchStatus / Resolved* / RecentItem / AddedBatch / UnresolvedTarget / AvailableBatch / AttachableBatch）
//! - `submit.rs`（P2 submit：SubmitDeliveryOut / SubmitOutcomeDto）
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。
//!
//! 2026-09-22 PR4 重构：参照 iam vo/ 范本从 dto.rs 拆出全部 Serialize 类型。

pub mod attach;
pub mod delivery_group;
pub mod delivery_note;
pub mod scan;
pub mod submit;

pub use attach::{AttachBatchConflict, AttachBatchesOut};
pub use delivery_group::{DeliveryGroupListOut, DeliveryGroupMemberOut, DeliveryGroupOut, UngroupedCustomerOut};
pub use delivery_note::{
    BatchDeliveryDetailData, DeliveryNoteCandidatePart, DeliveryNoteCandidatePartsOut,
    DeliveryNoteDetailOut, DeliveryNoteEventOut, DeliveryNoteLineItem, DeliveryNoteListOut,
    DeliveryNoteOut, DeliveryNotePickupListOut, DeliveryNotePickupScanOut,
};
pub use scan::{
    AddedBatchDto, AttachableBatchDto, AvailableBatchDto, BatchStatusDto, RecentItemDto,
    ResolvedEntityDto, ResolvedKindDto, ScanDeliveryNoteSummaryDto, ScanDeliveryOut, ScanOutcomeDto,
    UnresolvedTargetDto,
};
pub use submit::{SubmitDeliveryOut, SubmitOutcomeDto};