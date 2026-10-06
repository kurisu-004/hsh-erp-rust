//! prod::queue 域 —— 生产队列（工序候选池 + 工人持有 + 发放/移动/自动分配）
//!
//! ## 2026-10-08 重命名（worker_pool → queue，URL 硬切无 alias）
//!
//! - 目录 `src/modules/prod/worker_pool/` → `src/modules/prod/queue/`
//! - URL `/api/v2/prod/pool/*` → `/api/v2/prod/queue/*`（旧路径 404）
//! - 类型改名：`QueueService` / `QueueRepoTrait` / `QueueRepo` / `QueueCountsOut`
//!
//! 改名缘由：`worker_pool` 指的是旧实现里的一张中间视图，而本域的职责已经
//! 扩到「工序候选池 + 工人持有 + 队列发放」三块，`pool` 这个名字只覆盖了第一块。
//! `queue` 与前端页面语义（工序队列看板）一致，也给下述新端点留了名字空间。
//!
//! ## 端点（2026-10-08 收敛后）
//! - `GET  /api/v2/prod/queue/snapshot` —— 工序序列板（各工序候选数 + 待下发总数）
//! - `GET  /api/v2/prod/queue/processes/{process_id}` —— 单工序板（工人 + 持有 + 候选）
//! - `GET  /api/v2/prod/queue/pending` —— 待下发批次列表
//! - `POST /api/v2/prod/queue/dispatch` —— 下发批次到车间
//! - `POST /api/v2/prod/queue/auto-dispatch` —— 下发预览（只读）
//! - `POST /api/v2/prod/queue/recall` —— 召回（batch_id 走 body）
//! - `POST /api/v2/prod/queue/refill` —— 为工人抢满 max_held
//! - `POST /api/v2/prod/queue/move` —— 通用移动（POOL ↔ WORKER + WORKER ↔ WORKER）
//! - `POST /api/v2/prod/queue/auto-allocate` —— 按工序自动分配
//!
//! ## 路由注册顺序（axum matchit 硬约束，勿调换）
//! 1 段静态段（`/pending` `/dispatch` `/auto-dispatch` `/recall` `/refill` `/move`
//! `/auto-allocate` `/snapshot`）必须**全部**先于 `/{process_id}` 形状的动态段注册，
//! 否则 axum 把 `"counts"`、`"pending"` 之类的字面量当 process_id 解析。
//! `/processes/{process_id}` 是 2 段，与 1 段组不冲突。

pub mod board;
pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ===== 1 段静态段：必须全部先于下面任何动态段注册 =====
        // 发放流（2026-10-08 自 prod::batch 剥离：原本是 /prod/batches/pending 等）
        .route("/pending", get(handler::dispatch::list_pending))
        .route("/dispatch", post(handler::dispatch::dispatch))
        .route("/auto-dispatch", post(handler::dispatch::auto_dispatch))
        .route("/recall", post(handler::recall::recall))
        // 队列板聚合读（2026-10-08 新增，取代原 `/state` / `/counts` / `/{process_id}`）
        .route("/snapshot", get(handler::board::board_snapshot))
        // 写端点：refill / move / auto-allocate
        .route("/refill", post(handler::pool::admin_refill))
        .route("/move", post(handler::pool::move_batch))
        .route("/auto-allocate", post(handler::pool::auto_allocate))
        // ===== 2 段动态段：工序队列板详情 =====
        .route(
            "/processes/{process_id}",
            get(handler::board::board_process_detail),
        )
}
