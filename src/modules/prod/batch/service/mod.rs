//! prod::batch 域业务逻辑（按业务流聚合，`impl BatchService` 拆文件）
//!
//! 2026-10-02：`t_part_batch` 是生产执行单元，本模块承载**以批次为对象**的全部
//! 用例 —— 下发流 + 25 条批次路由的业务逻辑 + 它们共用的状态机 / OCC 守卫。
//! part 域只留「多批次动作」（`POST /api/v2/parts/{part_id}/cancel` /
//! `force-complete`）与 part 级动作（CRUD / 文件 / 各类 list）。
//!
//! ## 文件分工
//! - `dispatch.rs` —— 下发流：`list_pending` / `dispatch_batch`（bulk-only）/
//!   `auto_dispatch_preview`
//! - `transition.rs` / `transition_core.rs` —— to-XXX 三流（送检 / 通过 / 打回）
//!   的薄 wrapper + 批量聚合器，以及三个 `*_core` 共享核心与私有辅助
//! - `lifecycle.rs` —— 批次终态与返修起点（deliver / complete / start-repair）
//! - `shelf.rs` —— 上架 / 召回（place-on-shelf / recall-to-pending）
//! - `programming.rs` —— CNC 编程出口（release-from-programming）
//! - `outsource.rs` —— 外协流转三端点
//! - `repair.rs` —— 返修闭环两写点 + 三条集合读
//! - `batch_ops.rs` —— 拆批 / 取消批次
//! - `pickup.rs` —— 手动 pick-up
//! - `scan.rs` —— 扫码品检 / 司机扫码发货
//! - `worker_scan.rs` —— 工人扫码台主入口（RETURNED / INSPECTED 二合一）
//! - `list.rs` —— INSPECTION 状态批次集合读
//! - `guard.rs` —— 全部用例共用的自由函数：状态机守卫 / OCC / 货架校验 /
//!   status_gate 薄包装
//!
//! ## 签名约定
//! 形参一律 `<R: PartRepoTrait>(mut repo: R, ...)`：handler 开 tx 并 commit，
//! 生产 `R = &mut PgConnection`；跨域 repo 与 inline sqlx 走 `repo.conn_mut()`。
//! 用例读写 part 行（`get_part_inspected` / `insert_part_event`）与派生
//! （`part::statemachine`）都经 part 域 trait。
//!
//! ## 依赖方向：过渡期，**尚未单向**
//! 本模块的批次方法以 `<R: PartRepoTrait>` 形参接收 part 域 trait，批次写入经它
//! 落到 prod 域的表；而 `PartRepoTrait` 的默认体在 `part::repo` 内直接回调
//! `PartBatchRepo` + `status_gate`。即调用链是
//! `prod::batch::service → part::repo::PartRepoTrait → prod::batch::repo`：
//! **prod 域的 service 借 part 域的 trait 写 prod 域自己的表**，这是当前真实存在的
//! 反向依赖，不是单向。少数集合读与拆批直接调本域 `PartBatchRepo`（经
//! `repo.conn_mut()`），不改变上述借用关系。part → prod 方向另有 `status_gate` +
//! `PartBatchRepo` 两处数据依赖，代码调用点分布在 `part::service` 的 `batch` /
//! `crud` / `rollup` / `list_enrichment` / `phase1::events`。
//!
//! 收敛目标（分两步，本轮只做标注，未动代码）：
//! 1. prod service 改直调 `PartBatchRepo` / `status_gate`，不再借 part 域 trait；
//! 2. part 域的 `POST /api/v2/parts/{part_id}/cancel` 与 `/force-complete`
//!    改走 `prod::batch::service` / `status_gate` —— **前提是先把这 2 条端点重路由
//!    到 prod 域**，否则 part 域无法在不自建反向依赖的前提下完成多批次动作。
//!
//! ## 事务 / 角色守卫
//! 事务边界在 handler；角色守卫在 handler 与 service 双层（handler 做权限分发，
//! service 入口第一行再 `require_any_role`）。

#![allow(deprecated, clippy::too_many_arguments, clippy::type_complexity)]

pub mod batch_ops;
pub mod dispatch;
pub mod guard;
pub mod lifecycle;
pub mod list;
pub mod outsource;
pub mod pickup;
pub mod programming;
pub mod repair;
pub mod scan;
pub mod shelf;
pub mod transition;
pub mod transition_core;
pub mod worker_scan;

/// `prod::batch` service（ZST，与 worker_pool 范本一致）。
///
/// 公共方法均通过 `<BatchService>::method()` 访问（unit struct 形态）。
/// 显式 snowflake 形参保留（与 worker_pool `WorkerPoolService::refill_for_worker`
/// 同形 —— 跨模块调用方预留兼容）。
///
/// `impl` 块按业务流拆到各子文件（Rust 允许同一 `impl BatchService { ... }`
/// 分布在多个同 crate 文件中，编译器合并）。
pub struct BatchService;
