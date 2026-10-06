//! `prod::batch` repo 层 —— `t_part_batch` 的通用 SQL 真源
//!
//! `t_part_batch` 是生产执行单元，本表的 repo 层归 `prod::batch`。本文件现在只
//! 承载 3 个子模块的声明与重导出，逐个职责见下方「## 文件分工」。
//!
//! ## ZST `PartBatchRepo` —— `t_part_batch` 的通用 SQL 真源
//! 全仓读写批次表的默认入口（`queries.rs` 通用方法 + `sql.rs` 流转写点），
//! 跨域调用方一律走它。impl 目标是本域 ZST `PartBatchRepo`（**不是** part 域的
//! `PartRepo`）。
//!
//! ## 文件分工
//! - `queries.rs` —— ZST `PartBatchRepo` + 通用静态方法
//! - `sql.rs` —— inspection / lifecycle 流转的定位 + 写点
//! - `trait.rs` —— 胖 trait `PartBatchRepoTrait` + `impl for &mut PgConnection`
//!
//! 2026-10-07：待品检队列读（`GET /api/v2/prod/inspection/queue`）自本目录
//! `list.rs` 迁入 `prod::inspection`（该域是该页面的唯一数据源所在域，且迁后保持
//! 零跨域依赖）—— **`/repair` / `/repairing` 两条集合读的 SQL 在 service 层自建**
//! （`service/repair.rs::list_batches_matching` 内联），不在本目录。
//!
//! ## 已上移 / 迁出的两处
//! - `get_by_id` / `list_active_by_part_id` → `shared::batch::read`（跨域公共读取单元）；
//! - 原 ZST `BatchRepo`（只服务「PENDING 批次下发给车间」一条流，2026-10-02 去重后
//!   只剩 7 个专用方法）→ `prod::queue::repo::dispatch` 并改名 `QueueDispatchRepo`
//!   —— 唯一调用方是下发流 service，留在 batch 域等于让 queue 反向依赖 batch。
//!
//! ## 错误类型
//! repo 静态方法 → `sqlx::Error`（与项目惯例一致），由 service 层映射 `AppError`。

pub mod queries;
pub mod sql;
pub mod r#trait;

pub use queries::{NewInitialBatch, PartBatchRepo};
pub use r#trait::PartBatchRepoTrait;
