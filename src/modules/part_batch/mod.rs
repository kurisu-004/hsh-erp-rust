//! part_batch 域
//!
//! 对应 Python myERP：
//! - repository/part_batch_repository.py → `repo.rs`
//! - model/part_batch.py                  → `model.rs`
//!
//! Phase P1（送货分组）只挂 model + repo 两个文件；service / handler /
//! dto / statemachine 由 part_batch 域自身的实施阶段补齐。
//! 当前不在 `modules::v2_router` 下 nest 路由。

pub mod model;
pub mod repo;
pub mod repo_list;