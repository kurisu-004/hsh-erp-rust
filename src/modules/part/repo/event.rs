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

// 2026-10-12：`t_part_event` 的 INSERT（`sql::event_sql::insert_part_event`）已随该表
// 上提到 `crate::shared::part_event::insert`，本模块（含 `repo/sql/event_sql.rs`）随之
// 删除；这里保留空壳只为不破坏 `part::repo::event` 这个**模块路径**本身（仍有零调用方
// 的历史 import 形态，但已无 SQL 可转出）。
