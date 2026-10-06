//! part 域 SQL 真源（2026-09-22 PR2 拆分原 1771 行 sql.rs）
//!
//! ## 结构（2026-09-22 PR2 重构）
//!
//! 原 `sql.rs`（1771 行，超 conventions.md §2 1000 行硬上限）按"主表归属"
//! 拆分为 4 子文件：
//!
//! - `part_sql.rs` —— `t_part` 表 SQL（get_by_id / list_by_ids /
//!   get_by_serial / list_children / list_children_by_assemblies /
//!   get_part_inspected / get_part_detail /
//!   create_part / update_part / soft_delete_part / list_with_filters /
//!   count_with_filters / list_by_assembly_id / insert_child_for_assembly /
//!   cascade_sync_from_assembly / scale_children_quantity /
//!   get_part_rollup_state / update_part_rollup /
//!   clear_part_serial_no_when_completed / serial_prefix_for_customer /
//!   list_match_parts_by_keys / list_match_assemblies_by_keys /
//!   list_assembly_names_by_ids / update_order_info）
//! - `event_sql.rs` —— `t_part_event` 表 SQL（insert_part_event）
//! - `helper_sql.rs` —— 杂项 helper（`scale_qty` 缩放公式纯函数 + 单测）
//!
//! 2026-10-02：`batch_sql.rs`（`t_part_batch` 表 19 个流转写点）已随
//! `t_part_batch` 归属迁至 `crate::modules::prod::batch::repo::sql`（impl 目标由
//! 本文件的 ZST `PartRepo` 改为 `PartBatchRepo`）。本文件现承载 `t_part` +
//! `t_part_event` 两表的 SQL。
//!
//! ## 函数体零变化（PR2 约束）
//! 文件物理拆分，**所有 SQL 文本、函数体、签名、可见性、async 修饰保持不变**——
//! 仅按"主表归属"组织。sqlx prepare 哈希一致（`.sqlx/query-*.json` 无变化）。
//!
//! ## ZST `PartRepo`
//! 跨模块调用方（delivery_note / assembly / outsource / part_file / statistics /
//! shelf 6 域，prod::worker_pool 1 域）继续走 `PartRepo::xxx(&mut *conn, ...)`
//! 静态方法调用——保持 12 处静态调用零修改，故 ZST struct 提升至本 mod.rs
//! （统一暴露），各 sql_* 子文件 `impl PartRepo { ... }` 拼装。

/// part 域 ZST struct（承载 `t_part` + `t_part_event` 两表全部固有静态方法；
/// `t_part_batch` 的静态方法 2026-10-02 起在 `prod::batch::repo`）。
///
/// 跨模块调用方（delivery_note / assembly / outsource / part_file / statistics /
/// shelf / prod::worker_pool 共 7 域）继续走 `PartRepo::xxx(&mut *conn, ...)`
/// 静态方法调用——本任务**不能**破坏 `part::repo::PartRepo` 作为 ZST 的对外身份。
pub struct PartRepo;

pub mod event_sql;
pub mod helper_sql;
pub mod part_sql;

// 重导出保留原路径兼容（part/repo/mod.rs 已 `pub use sql::{...}`，继续穿透）。
pub use helper_sql::scale_qty;
pub use part_sql::{
    AssemblyMatchRow, ChildInheritFields, NewPartCreate, PartListFilters, PartUpdate,
};
