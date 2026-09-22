//! `t_part_event` 事件日志（2026-09-22 D-6 重构后**重导出壳**）
//!
//! ## 历史背景
//! 2026-09-22 D-6 重构把原 `repo/part.rs` + `repo/batch.rs` + `repo/event.rs`
//! 三文件 SQL 全部合并到 `repo/sql.rs` 的单 ZST `PartRepo`，并新增胖 trait
//! `PartRepoTrait`（在 `mod.rs`）。
//!
//! 本文件保留为**重导出壳**，让跨模块调用方的 `use crate::modules::part::repo::event::*`
//! （如有）路径仍可解析。
//!
//! ## 本任务不修改 SQL 字符串
//! 所有 SQL 字符串都在 `sql.rs` 内（ZST `PartRepo` 固有静态方法），保持原样。

// `t_part_event` INSERT 方法 `PartRepo::insert_part_event` 已在 `sql.rs` 内合并。
// 此模块当前为空（保留模块路径兼容），如未来需要拆 `EventRepo` ZST 再启用。