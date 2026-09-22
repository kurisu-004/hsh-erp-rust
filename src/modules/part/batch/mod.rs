//! part_batch 域
//!
//! 2026-09-22 PR2 合并：原 `part_batch` 域（1866 行 helper，无独立 URL，5 域
//! 静态调用）物理合并入 part 域的 `part/batch/` 子目录。
//!
//! ## 结构
//! - `model.rs`：`TPartBatch` / `RecentBatchRow` / `PartBatchScanRow` /
//!   `InspectionBatchListRow` 行结构（sqlx `FromRow`）。
//! - `repo.rs`：原 `part_batch/repo/{mod, sql, list}.rs` 三文件内容合并为
//!   单文件——ZST `PartBatchRepo` + 17 个 pub 静态方法 + 胖 trait
//!   `PartBatchRepoTrait` + `impl for &mut PgConnection`。
//!
//! ## 不再保留独立路由
//! `modules::v2_router` 不 nest 本子模块；callers 走 `crate::modules::part::batch::*`。
//!
//! ## 跨模块调用方（11 处 import 同步）
//! 原 `crate::modules::part_batch::repo::*` / `model::*` 全部改写到
//! `crate::modules::part::batch::repo::*` / `model::*`。11 处分布：
//! task/auto_complete / part/dto.rs / part/service/{batch, crud,
//! inspection, inspection_core, list_enrichment, phase1/events,
//! phase1/batch_ops}.rs / part/repo/{mod, sql}.rs /
//! prod/worker_pool/repo/mod.rs。详见 commit message。

pub mod model;
pub mod repo;

// 重导出 model 与 repo 的公开符号，保持外部 callers 用 `part::batch::*` 一层路径。
pub use model::{InspectionBatchListRow, PartBatchScanRow, RecentBatchRow, TPartBatch};
pub use repo::{NewInitialBatch, PartBatchRepo, PartBatchRepoTrait};
