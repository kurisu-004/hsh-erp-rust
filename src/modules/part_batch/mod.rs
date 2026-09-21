//! part_batch 域
//!
//! 对应 Python myERP：
//! - repository/part_batch_repository.py → `repo/`（sql.rs + list.rs）
//! - model/part_batch.py                  → `model.rs`
//!
//! Phase P1（送货分组）只挂 model + repo 两个文件；service / handler /
//! dto / statemachine 由 part_batch 域自身的实施阶段补齐。
//! 当前不在 `modules::v2_router` 下 nest 路由。
//!
//! 2026-09-22 D-1 重构：`repo.rs` + `repo_list.rs` → `repo/{mod, sql, list}.rs`，
//! 抽 `PartBatchRepoTrait` 胖 trait 直接 `impl for &mut PgConnection`，对齐 iam /
//! shelf / customer 范本。

pub mod model;
pub mod repo;