//! 货架子模块 service 子模块聚合
//!
//! 2026-10-10：picker 端点（for-return / for-inspection）随自动选架下线，
//! `picker.rs` 随之删除 —— 本子模块现在只剩 `crud`（list / get / create / update /
//! soft_delete，含 `ShelfService` struct）。
//!
//! 2026-10-02 域拆分：per-shelf mapping（`set_shelf_processes` /
//! `list_shelf_processes` / `list_all_process_mappings`）整体搬到
//! `crate::modules::prod::shelf_process`，与本 service 不再平级；本 service 收敛为
//! 纯 `t_shelf` 的 7 端点业务逻辑。
//!
//! ## 调用方契约
//! `handler.rs` 仅引 `crate::modules::iam::shelf::service::ShelfService::*`，
//! 不直接访问 `crud`。本模块用 `pub use crud::*` 把 `ShelfService` 类型重新汇出到
//! `service` 命名空间。
//!
//! ## 共享常量
//! `DEFAULT_LIMIT` / `MAX_LIMIT` / `ZONE_PRODUCTION` / `ZONE_INSPECTION`
//! 默认可见性（private to module）。`crud` 是 `service` 的子模块，Rust 规则下子模块
//! 可见父模块的 private 项，无需 `pub` 修饰。

pub mod crud;

pub use crud::ShelfService;

// ---------------------------------------------------------------------------
// 共享常量
// ---------------------------------------------------------------------------

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

const ZONE_PRODUCTION: &str = "PRODUCTION";
const ZONE_INSPECTION: &str = "INSPECTION";
