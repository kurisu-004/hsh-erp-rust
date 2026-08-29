//! part 域业务逻辑（按业务流聚合）
//!
//! - `inspection.rs`：to_ship / to_inspection / to_process 流薄 wrapper +
//!   批量聚合器 + 共享私有辅助函数（Phase F2 / F3）
//! - `inspection_core.rs`：to_ship / to_inspection / to_process 的 `*_core`
//!   共享核心（impl 块拆文件，承接 inspection.rs 因 1000 行上限而拆出的逻辑）
//! - `worker_scan.rs`：`POST /parts/worker-scan` 流（Task 8）；impl 块拆文件，
//!   Rust 允许同一 `impl PartService { ... }` 分布在多个同 crate 文件中。
//! - `crud.rs`：create / list / detail / update / soft-delete / by-serial /
//!   batch-create / upload-drawing
//! - `lifecycle.rs`：deliver / cancel / complete / start_repair

pub mod crud;
pub mod inspection;
pub mod inspection_core;
pub mod lifecycle;
pub mod worker_scan;

// 重导出子模块内的 `pub const`（impl 块里的方法由 `PartService` 自身承载，
// 不需要 re-export；caller 通过 `PartService::xxx()` 直接调用）。
pub use inspection::{BATCH_TO_INSPECTION_MAX_ITEMS, BATCH_TO_SHIP_MAX_ITEMS};

/// 批量端点单次请求最大 item 数（handler/service 双层校验）。
pub const BATCH_CREATE_PARTS_MAX_ITEMS: usize = 200;

pub struct PartService;