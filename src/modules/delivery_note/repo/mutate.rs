//! `t_delivery_group` / `t_delivery_note` / `t_delivery_note_event` 写查询
//! （INSERT / UPDATE / DELETE）。
//!
//! ## 历史背景
//! 2026-09-22 D-5 重构把原 `repo/query.rs` + `repo/mutate.rs` 两文件 SQL 全部合并到
//! `repo/sql.rs` 的三个 ZST（`DeliveryGroupRepo` / `DeliveryNoteRepo` /
//! `DeliveryNoteEventRepo`），并新增胖 trait `DeliveryNoteRepoTrait`（在 `mod.rs`）。
//!
//! 本文件保留为**重导出壳**，让潜在 caller 的
//! `use crate::modules::delivery_note::repo::mutate::xxx` 路径仍可解析。这是
//! 2026-09-22 shelf / customer / part_batch 范本同形做法（见 `part/repo/part.rs`）。
//!
//! ## 本任务不修改 SQL 字符串
//! 所有 SQL 字符串都在 `sql.rs` 内（ZST 固有静态方法），保持原样。
//!
//! 2026-09-22：原 `query.rs` + `mutate.rs` 全文内容已搬迁至 `sql.rs`。

// 重导出 mod.rs 内的 ZST struct（写查询面）。所有 SQL 真源均在 sql.rs，
// 本文件不再承载任何 sqlx::query! 调用。
pub use super::DeliveryGroupRepo as _DeliveryGroupRepoMutate;
pub use super::DeliveryNoteEventRepo as _DeliveryNoteEventRepoMutate;
pub use super::DeliveryNoteRepo as _DeliveryNoteRepoMutate;