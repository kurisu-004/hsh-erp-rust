//! 业务域聚合：按 REST/mcp/ws 三种入口组装 Router
//!
//! 重构版业务 REST 接口统一挂在 `/api/v2`；AI 只读入口 `/api/mcp` 保持非版本化；
//! WebSocket 大屏挂在 `/ws/dashboard`。

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

pub mod applicant;
pub mod assembly;
pub mod auth;
pub mod cnc_program;
pub mod customer;
pub mod dashboard;
pub mod delivery_note;
pub mod outsource;
pub mod part;
pub mod part_batch;
pub mod part_file;
pub mod process;
pub mod shelf;
pub mod statistics;
pub mod user;
pub mod worker;
pub mod worker_pool;
pub mod work_type;

#[derive(Serialize)]
struct HealthResp {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

async fn health(State(_state): State<Arc<AppState>>) -> Json<HealthResp> {
    Json(HealthResp {
        status: "ok",
        service: "hsh-erp-api",
        version: "v2",
    })
}

/// `/api/v2/*` 业务路由聚合（重构版统一版本前缀）
pub fn v2_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health))
        .nest("/auth", auth::router())
        .nest("/users", user::router())
        .nest("/customers", customer::router())
        .nest("/applicants", applicant::router())
        .nest("/workers", worker::router())
        .nest("/work-types", work_type::router())
        .nest("/processes", process::router())
        .nest("/shelves", shelf::router())
        .nest("/parts", part::router())
        .nest("/assemblies", assembly::router())
        .nest("/cnc-programs", cnc_program::router())
        .nest("/part-files", part_file::router())
        .nest("/outsource", outsource::router())
        .nest("/delivery-notes", delivery_note::router())
        .nest("/delivery-groups", p1_router())
        .nest("/statistics", statistics::router())
        .nest("/worker-pool", worker_pool::router())
        .nest("/admin/worker-pool", worker_pool::admin_router())
}

/// `/ws/*` WebSocket 入口（当前仅 dashboard 大屏）
pub fn ws_router() -> Router<Arc<AppState>> {
    dashboard::router()
}

/// P1 送货分组 router re-export（供 `/api/v2/delivery-groups` nest 使用）
pub fn p1_router() -> Router<Arc<AppState>> {
    delivery_note::handler::p1_router()
}