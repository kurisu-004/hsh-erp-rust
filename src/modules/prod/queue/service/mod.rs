//! prod::queue service（unit struct + 按业务流拆文件）
//!
//! ## 业务流分片
//! - [`queue`] —— 队列写端点用例（`refill_for_worker` / `move_batch` /
//!   `auto_allocate_for_process`）；原 `service.rs` 整文件搬入
//! - [`dispatch`] —— 下发流 3 用例（`list_pending` / `dispatch_batch` /
//!   `auto_dispatch_preview`）；2026-10-08 自 `prod::batch::service::dispatch` 搬入
//! - [`recall`] —— 召回 1 用例（`recall_to_pending`）；2026-10-08 自
//!   `prod::batch::service::shelf` 抽出
//!
//! ## Service 形态
//! [`queue::QueueService`] 保持 unit struct（**不**持字段依赖）。snowflake 由每个
//! 写方法形参显式收 —— 跨域调用点（`prod::batch::service::worker_scan` 的
//! worker-scan 路径直接调 `QueueService::refill_for_worker_with_work_type`）
//! 沿用 ZST 静态 + 显式 snowflake 的旧形态，改成 trait 注入式要动 batch 域。
//!
//! ## 事务 / 角色守卫
//! 事务边界在 handler；角色守卫在 service 入口第一行（handler 只做权限分发）。

#![allow(deprecated, clippy::too_many_arguments, clippy::type_complexity)]

pub mod dispatch;
pub mod queue;
pub mod recall;

pub use queue::QueueService;
