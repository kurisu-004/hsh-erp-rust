//! worker_pool 域
//!
//! 2026-09-30 重构：worker-pool → pool 路径收敛。
//! - 全部端点挪到 `/api/v2/prod/pool/*`
//! - 5 个端点一次性挂在 `router()`：`/state`、`/counts`、`/{process_id}`、
//!   `/refill`、`/move`、`/auto-allocate`
//! - 原 `admin_router()` 已被合并入主 router，前端旧 `/admin/worker-pool/...`
//!   路径 404（router 层不再挂载）
pub mod dto;
pub mod handler;
pub mod model;
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
        // 读端点（无 role guard / admin 视角）
        .route("/state", get(handler::state))
        // 2026-09-30 新增：全工序候选批次聚合计数（dashboard 快照型查询）。
        // 路由段 `/counts` 必须在 `/{process_id}` 之前注册，否则 axum 会把
        // "counts" 当 process_id 解析。
        .route("/counts", get(handler::pool_counts))
        .route("/{process_id}", get(handler::pool_by_process))
        // 写端点：refill / move / auto-allocate
        .route("/refill", post(handler::admin_refill))
        // 2026-09-30 新增：通用 move 端点（覆盖 POOL ↔ WORKER + WORKER ↔ WORKER 三方向）
        .route("/move", post(handler::move_batch))
        .route("/auto-allocate", post(handler::auto_allocate))
}
