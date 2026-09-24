//! 集成测试 fixture 目录模块（PR13 Phase A 引入，2026-09-23）
//!
//! 历史：`test-support/src/fixture.rs` 单文件（97 行）随 Phase F 范本引入，
//! 2026-09-23 PR13 Phase A 拆为目录模块，2026-09-23 PR13 Phase G 新增
//! `part` 子模块（11 个 sub-file 复用）+ `delivery` 子模块（5 个 sub-file 复用），
//! 2026-09-24 PR13 Phase H 新增 `production` / `assembly` / `shelf` /
//! `statistics` / `outsource` 子模块，2026-09-24 PR13 Phase I 新增 `iam`
//! 子模块（tests/iam/ 3 sub-file 复用）+ `user_repo` 子模块
//! （tests/user_repo/ 4 sub-file 复用）；2026-09-24 PR13 Phase I 末段新增
//! 10 个单文件 binary 子模块（applicant / customer / dashboard_ws / _e2e /
//! cnc_program / auto_complete / guard_dn_in_use / idempotency / cos_opendal
//! / cos_real_smoke）。
//!
//! ## 子模块命名约定
//! - 每个子文件名 = 对应 binary 名（如 `part.rs` 服务 `cargo nextest run --filter-binary part`）
//! - 每个子模块导出 `<Binary>Fixture` struct + `load_<binary>_fixture` 函数
//! - `pub use <sub>::*` 在本 mod.rs 重新导出，让 crate-root 可访问
//!   （`hsh_erp_test_support::load_part_fixture` 直接可用）
//!
//! ## 与 `fixtures`（动态 helper）的分工
//! - `fixtures`：动态 INSERT helper（`insert_user_with_password` / `add_role` 等），
//!   计划 PR-C.Final 全部迁出后删除（PR-C 末删 `test-support/src/fixtures.rs`）
//! - `fixture`（本目录）：预制 SQL 静态行集合，PR13 Phase F 引入的范本

pub mod _e2e;
pub mod applicant;
pub mod assembly;
pub mod auto_complete;
pub mod cnc_program;
pub mod cos_opendal;
pub mod cos_real_smoke;
pub mod customer;
pub mod dashboard_ws;
pub mod delivery;
pub mod guard_dn_in_use;
pub mod iam;
pub mod idempotency;
pub mod outsource;
pub mod part;
pub mod process_chain;
pub mod production;
pub mod shelf;
pub mod statistics;
pub mod user_repo;
pub use _e2e::*;
pub use applicant::*;
pub use assembly::*;
pub use auto_complete::*;
pub use cnc_program::*;
pub use cos_opendal::*;
pub use cos_real_smoke::*;
pub use customer::*;
pub use dashboard_ws::*;
pub use delivery::*;
pub use guard_dn_in_use::*;
pub use iam::*;
pub use idempotency::*;
pub use outsource::*;
pub use part::*;
pub use process_chain::*;
pub use production::*;
pub use shelf::*;
pub use statistics::*;
pub use user_repo::*;