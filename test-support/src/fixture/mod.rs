//! 集成测试 fixture 目录模块（PR13 Phase A 引入，2026-09-23）
//!
//! 历史：`test-support/src/fixture.rs` 单文件（97 行）随 Phase F 范本引入，
//! 2026-09-23 PR13 Phase A 拆为目录模块，未来 Phase G/H/I 增补
//! `part` / `delivery` / `production` / `assembly` / `shelf` / `statistics`
//! / `outsource` / `iam` / `user_repo` / 10 个单文件 binary 子模块。
//!
//! ## 子模块命名约定
//! - 每个子文件名 = 对应 binary 名（如 `part.rs` 服务 `cargo nextest run --filter-binary part`）
//! - 每个子模块导出 `<Binary>Fixture` struct + `load_<binary>_fixture` 函数
//! - `pub use <sub>::*` 在本 mod.rs 重新导出，让 crate-root 可访问
//!   （`hsh_erp_test_support::load_part_fixture` 直接可用）
//!
//! ## 与 `fixtures`（动态 helper）的分工
//! - `fixtures`：动态 INSERT helper（`insert_user_with_password` / `add_role` 等），
//!   计划 PR13 Phase I 全部迁出后删除（PR-C 末删 `test-support/src/fixtures.rs`）
//! - `fixture`（本目录）：预制 SQL 静态行集合，PR13 Phase F 引入的范本

pub mod process_chain;
pub use process_chain::*;
