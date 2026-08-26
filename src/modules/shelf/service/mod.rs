//! shelf 域 service 子模块聚合
//!
//! 拆分依据（crud-five-domains plan）：把单文件 `service.rs` 按职责拆为
//! - `crud`     —— list/get/create/update/soft_delete（含 `ShelfService` struct）
//! - `picker`   —— for-return / for-inspection / 全集 process 映射
//!
//! 让每个文件都落在 < 800 行（conventions.md §2）。
//!
//! per-shelf mapping（`set_shelf_processes` / `list_shelf_processes`）单独在
//! `crate::modules::shelf::process_mapping`，与本 service 平级，调用方走
//! `ShelfProcessService::*`。
//!
//! ## 调用方契约
//! `handler.rs` 仅引 `crate::modules::shelf::service::ShelfService::*`，
//! 不直接访问 `crud` / `picker`。本模块用 `pub use crud::*` 把 `ShelfService`
//! 类型重新汇出到 `service` 命名空间；`impl ShelfService` 块写在 `picker.rs`
//! 里不影响方法可见性 —— Rust 的 inherent 方法按类型名寻址，不依赖定义所在文件。
//!
//! ## 共享常量
//! `DEFAULT_LIMIT` / `MAX_LIMIT` / `ZONE_PRODUCTION` / `ZONE_INSPECTION`
//! 默认可见性（private to module）。`crud` / `picker` 是 `service` 的子模块，
//! Rust 规则下子模块可见父模块的 private 项，无需 `pub` 修饰。

pub mod crud;
pub mod picker;

pub use crud::ShelfService;

// 子模块自己的 impl ShelfService 块没新 pub 项；这行只为满足 plan 「re-export
// everything that callers expect」契约（无副作用）。
#[allow(unused_imports)]
pub use picker::*;

// ---------------------------------------------------------------------------
// 共享常量
// ---------------------------------------------------------------------------

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

const ZONE_PRODUCTION: &str = "PRODUCTION";
const ZONE_INSPECTION: &str = "INSPECTION";