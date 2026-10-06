//! `t_part_batch` repo 的**空壳** —— 仅保留模块路径，无任何代码项。
//!
//! 2026-10-02 域迁移：`t_part_batch` 的 repo 层已整体归 `prod::batch`。批次查询与
//! 状态机写点的真源是 `prod::batch::repo::PartBatchRepo`（通用方法见
//! `repo/queries.rs`、流转写点见 `repo/sql.rs`），「PENDING 下发」专用查询见
//! `prod::batch::repo::BatchRepo`。
//! `part::repo::PartRepo` **不再持有任何批次方法**。
//!
//! 本文件既不重导出也不引用上述符号，只让 `crate::modules::part::repo::batch`
//! 路径继续可解析；新增批次 SQL 一律加在 `prod::batch::repo`，不要在此复活。

// 空模块：`t_part_batch` 的查询 / mark 方法全部在 `prod::batch::repo::PartBatchRepo`。
