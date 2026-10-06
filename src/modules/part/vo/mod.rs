//! part 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto` / `super::dto_crud`。
//!
//! 按端点语义拆为：
//! - `part.rs`：单 part 详情 / 列表 / 事件
//! - `part_batch.rs`：part batch 详情 / 列表 / 子域相关（扫描 / 批量创建 / 批量更新）
//! - `location_tree.rs`：位置树（`GET /api/v2/parts/location-tree`）
//!
//! 2026-10-02：inspection 端点出参（`inspection.rs` 整文件）与 lifecycle 出参
//! （`lifecycle.rs` 的 to-XXX / batch-to-XXX / worker-scan 五类）随批次用例迁往
//! `crate::modules::prod::batch::vo`。本模块只留 part 级端点的出参。
//!
//! ## 与 `super::dto` / `super::dto_crud` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

pub mod location_tree;
pub mod part;
pub mod part_batch;

pub use location_tree::{LocationTreeNodeOut, LocationTreeOut};
pub use part::{
    ChainState, PartBatchListItemOut, PartDetailOut, PartEventOut, PartListItem, PartListOut,
    PartOut, PendingProgrammingOut,
};
pub use part_batch::{
    BatchUpdateOrderInfoFailure, BatchUpdateOrderInfoOut, ExcelMatchType, MatchByExcelItemResult,
    PartBatchCreateFailure, PartBatchCreateOut, PartBatchScanOut, PartMatchInfoOut,
    PartScanContextOut, PartScanInfoOut,
};
