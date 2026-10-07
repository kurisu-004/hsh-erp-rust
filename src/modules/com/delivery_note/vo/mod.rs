//! delivery_note 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为：
//! - `delivery_group.rs`（送货分组：DeliveryGroup* / UngroupedCustomerOut）
//! - `delivery_note.rs`（送货单 CRUD：DeliveryNoteOut / ListOut / LineItem /
//!   DetailOut / BatchDeliveryDetailData）
//! - `batch_status.rs`（`BatchStatusDto`：批次状态强类型投影）
//! - `scan_tree.rs`（扫码三层树：DeliveryScanTreeOut / Assembly / Part / Batch /
//!   Draft / PerSetPart）
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。
//!
//! 2026-09-22 PR4 重构：参照 iam vo/ 范本从 dto.rs 拆出全部 Serialize 类型。
//!
//! ## 域内自用（2026-10-08 新增）
//! VO 字段类型只允许来自本 `vo/` 子目录或 `chrono`。跨域数据（`TPart` / `TAssembly` /
//! `TCustomer` / `TPartBatch` / `TWorker` / `TWorkType`）必须在 service 层摊平成域内
//! 标量字段后再装配，VO 里不出现他域行模型，也不 `pub use` / `pub type` 别名指向
//! 他域类型。现状：全仓实测零违规（`vo/` 下的 `use` 只有 `serde` / `chrono` /
//! 本域兄弟模块）。

pub mod batch_status;
pub mod delivery_group;
pub mod delivery_note;
pub mod scan_tree;

pub use batch_status::BatchStatusDto;
pub use delivery_group::{
    DeliveryGroupListOut, DeliveryGroupMemberOut, DeliveryGroupOut, UngroupedCustomerOut,
};
pub use delivery_note::{
    BatchDeliveryDetailData, DeliveryNoteDetailOut, DeliveryNoteLineItem, DeliveryNoteListOut,
    DeliveryNoteOut,
};
pub use scan_tree::{
    DeliveryScanAssemblyOut, DeliveryScanBatchOut, DeliveryScanDraftOut, DeliveryScanPartOut,
    DeliveryScanPerSetPartOut, DeliveryScanTreeOut,
};
