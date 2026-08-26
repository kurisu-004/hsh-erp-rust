//! part 域业务逻辑（按业务流聚合）
//!
//! - `inspection.rs`：pass / fail / scan 流（既有，完整迁入）
//! - `crud.rs`：create / list / detail / update / soft-delete / by-serial /
//!   batch-create / upload-drawing
//! - `lifecycle.rs`：deliver / cancel / complete / start_repair

pub mod crud;
pub mod inspection;
pub mod lifecycle;

// 重导出子模块内的 `pub const`（impl 块里的方法由 `PartService` 自身承载，
// 不需要 re-export；caller 通过 `PartService::xxx()` 直接调用）。
pub use inspection::{BATCH_PASS_INSPECTION_MAX_ITEMS, BATCH_SCAN_INSPECT_MAX_ITEMS};

/// 批量端点单次请求最大 item 数（handler/service 双层校验）。
pub const BATCH_CREATE_PARTS_MAX_ITEMS: usize = 200;

pub struct PartService;