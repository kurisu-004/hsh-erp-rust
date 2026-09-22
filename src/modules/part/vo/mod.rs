//! part 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto` / `super::dto_crud`。
//!
//! 按端点语义拆为：
//! - `part.rs`：单 part 详情 / 列表 / 事件
//! - `part_batch.rs`：part batch 详情 / 列表 / 子域相关（扫描 / 批量创建 / 批量更新）
//! - `lifecycle.rs`：lifecycle state 出参（to-XXX / worker-scan / 位置树）
//! - `inspection.rs`：inspection 端点出参（inspection-batches / repair-batches / repairing-batches）
//!
//! ## 与 `super::dto` / `super::dto_crud` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

pub mod inspection;
pub mod lifecycle;
pub mod part;
pub mod part_batch;

pub use inspection::{InspectionBatchListItemOut, InspectionBatchListOut, RepairBatchesOut};
pub use lifecycle::{
    BatchOpFailure, BatchToXxxOut, LocationTreeNodeOut, LocationTreeOut, ToXxxOut,
    WorkerScanCoreOut, WorkerScanOut,
};
pub use part::{
    PartBatchListItemOut, PartDetailOut, PartEventOut, PartListItem, PartListOut, PartOut,
    PendingProgrammingOut,
};
pub use part_batch::{
    BatchUpdateOrderInfoFailure, BatchUpdateOrderInfoOut, MatchByExcelItemResult,
    PartBatchCreateFailure, PartBatchCreateOut, PartBatchScanOut, PartScanContextOut,
    PartScanInfoOut,
};