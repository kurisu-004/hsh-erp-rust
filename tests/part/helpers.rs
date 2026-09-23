//! part 域 sub-file 共享的 helper 入口（thin barrel）
//!
//! 2026-09-23 PR13 Phase G：原 396 行本地 helper（send / json_request / setup /
//! login_* / insert_l1 / insert_l2 / insert_part_with_status / insert_batch /
//! batch_version / setup_inspection_and_production_shelves / seed_process 等）
//! 全部迁出：
//! - HTTP helper（send / json_request / login_token）── `hsh_erp_test_support::http`
//! - 角色登录（login_*）── 由 `load_part_fixture` 静态 SQL + `bootstrap_as_*` 替代
//! - 域 fixture（insert_l1 / insert_l2 / insert_part_with_status / insert_batch /
//!   setup_inspection_and_production_shelves / seed_process）──
//!   `hsh_erp_test_support::fixture::PartFixture` 提供常量 ID + 子测试按需
//!   用 sqlx::query 直插
//! - batch_version —— 子测试 inline `sqlx::query_scalar`（一行调用）
//!
//! 本文件保留为 `pub use hsh_erp_test_support::*;` thin barrel：
//! - 子文件 `mod helpers; use helpers::*;` 路径不变，继续命中本 barrel
//! - 子文件 `use hsh_erp_test_support::fixture::PartFixture;` 拿 fixture struct
//!
//! `tests/part/serial.rs` / `tests/part/file.rs` 不用本 barrel（直引 test-support）；
//! 其余 10 个 sub-file 全走本 barrel。

#![allow(dead_code, unused_imports)]

pub use hsh_erp_test_support::*;