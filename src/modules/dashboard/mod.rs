pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use axum::routing::get;
use std::sync::Arc;

/// `/ws/*` 入口（WebSocket）：当前唯一端点 `/ws/dashboard`
///
/// 在 `modules::ws_router()` 下挂 `/ws` 前缀（不带 `/api/v2`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/dashboard", get(handler::ws_dashboard))
}

pub fn http_router() -> Router<Arc<AppState>> {
    Router::new().route("/snapshot", get(handler::get_snapshot))
}
