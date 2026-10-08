//! prod::scan 域业务逻辑（`impl ScanService` 拆文件）
//!
//! 2026-10-10 自 `prod::worker` / `prod::batch` 搬入：报工台的 4 条写路径按
//! **前端消费方**（`views/production/scan/` 三页 + 扫工牌弹窗）归到一域，不再散落在
//! `prod::worker` / `prod::batch` / `part` 三处。
//!
//! ## 文件分工
//! - `badge.rs` —— 扫工牌（`POST /scan/verify-badge`）
//! - `worker_scan.rs` —— 报工台主入口（`POST /scan/worker-scan`，RETURNED /
//!   INSPECTED 二合一 + 链尾自动送检 + 同事务 refill 编排所需的核心）
//! - `pickup.rs` —— 手动 pick-up（`POST /scan/batches/{batch_id}/pick-up`，
//!   含部分领取自动拆批）
//!
//! 两条只读聚合端点（`/scan/pickable` / `/scan/held`）**不在本目录**：它们在
//! [`super::listing`] 子模块里（该子模块受域隔离护栏覆盖，必须零跨域依赖）。
//!
//! ## 签名约定
//! 形参一律 `<R: PartRepoTrait>(mut repo: R, ...)`：handler 开 tx 并 commit，
//! 生产 `R = &mut PgConnection`；跨域 repo 与 inline sqlx 走 `repo.conn_mut()`。
//! 纯仓储调用（如扫工牌按 `badge_code` 反查）直接收 `&mut PgConnection`，
//! 因为它不写任何批次表。
//!
//! ## 事务 / 角色守卫
//! 事务边界在 handler；角色守卫在 handler 与 service 双层（handler 做权限分发，
//! service 入口再 `require_any_role`）。
//!
//! ## 依赖方向
//! 本域是**转发型**域：`worker_scan` 必然 import `part`（repo / 事件日志 /
//! 状态机）、`assembly`（父件级联）、`iam::shelf`（经 `shared::shelf`）、
//! `prod::queue`（refill）、`prod::worker`（按工牌反查）。因此**整域不适用**
//! `shared::domain_guard`，只有 `listing/` 子模块单独装护栏。

#![allow(deprecated, clippy::too_many_arguments, clippy::type_complexity)]

pub mod badge;
pub mod pickup;
pub mod worker_scan;

/// `prod::scan` service（ZST，与 `prod::batch` / `prod::queue` 的范本一致）。
///
/// 公共方法均通过 `<ScanService>::method()` 访问（unit struct 形态）。
/// 显式 snowflake 形参保留（与 `QueueService::refill_for_worker` 同形 ——
/// 跨模块调用方预留兼容）。
pub struct ScanService;
