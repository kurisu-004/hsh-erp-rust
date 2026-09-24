//! part 域 integration test binary 的 sub-file 共享 thin barrel（2026-09-24 PR13 Phase C.Final）
//!
//! 11 个 part sub-file（batch / crud / file / inspection_batches / lifecycle /
//! list_enrichment / repair / serial / to_inspection / to_process / to_ship）
//! 仍保留 `#[path = "helpers.rs"] mod helpers;` 声明（PR13 Phase G 预期清
//! 末，但实际未执行——part binary 不在 PR-C.C.Final scope 内）。
//!
//! 本文件原本承载 396 行 part 域动态 helper（setup / login_manager /
//! insert_l1 / insert_part_with_status / insert_batch / setup_inspection_and_production_shelves
//! 等），多数已被 sub-file 改走 `hsh_erp_test_support::*` 直引，本文件不再被
//! 任何 sub-file 通过 `helpers::*` 调用（grep verify 0 命中）。
//!
//! PR13 Phase C.Final 改动：
//! - 本文件由「full 396 行 part helper 集合」压缩为 thin barrel
//!   `pub use hsh_erp_test_support::*;`。
//! - 删除对 `fixtures.rs` 的依赖（`add_role` / `insert_user_with_password` /
//!   `clean_db` 等 PR-C 末承诺删除的动态 helper），让 `tests/part/*` 11 个
//!   sub-file 在 `fixtures.rs` 删除后仍能编译。
//!
//! 11 个 part sub-file 后续可在独立 PR 内清理 `mod helpers;`（届时本文件可
//! 一并删除）；本任务不在该 scope。

#![allow(dead_code, clippy::duplicate_mod, unused_imports)]

pub use hsh_erp_test_support::*;