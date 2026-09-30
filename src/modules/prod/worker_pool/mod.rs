//! worker_pool 域
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
        .route("/state", get(handler::state))
        // 2026-09-30 新增：全工序候选批次聚合计数（dashboard 快照型查询）。
        // 路由段 `/counts` 必须在 `/{process_id}` 之前注册，否则 axum 会把
        // "counts" 当 process_id 解析。
        .route("/counts", get(handler::pool_counts))
        .route("/{process_id}", get(handler::pool_by_process))
}

pub fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/refill", post(handler::admin_refill))
        .route("/remove", post(handler::admin_remove))
        .route("/auto-allocate", post(handler::auto_allocate))
        // 2026-09-14 follow-up-ux 新增：单 batch 拖拽分配
        .route("/assign", post(handler::admin_assign))
}
