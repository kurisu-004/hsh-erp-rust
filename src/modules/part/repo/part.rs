//! `t_part` 主表查询 + 状态机 UPDATE（2026-09-22 D-6 重构后**重导出壳**）
//!
//! ## 历史背景
//! 2026-09-22 D-6 重构把原 `repo/part.rs` + `repo/batch.rs` + `repo/event.rs`
//! 三文件 SQL 全部合并到 `repo/sql.rs` 的单 ZST `PartRepo`，并新增胖 trait
//! `PartRepoTrait`（在 `mod.rs`）。
//!
//! 本文件保留为**重导出壳**，让跨模块调用方（assembly / delivery_note 等 7 域）
//! 的 `use crate::modules::part::repo::part::{NewPartCreate, PartListFilters,
//! PartUpdate}` 等路径仍可解析。这是 2026-09-22 shelf / customer / part_batch
//! 范本同形做法。
//!
//! ## 本任务不修改 SQL 字符串
//! 所有 SQL 字符串都在 `sql.rs` 内（ZST `PartRepo` 固有静态方法），保持原样。

pub use super::sql::{ChildInheritFields, NewPartCreate, PartListFilters, PartUpdate};