//! dashboard 域（WebSocket 大屏）
//!
//! 对应 Python myERP/api/v1/ws.py。端点路径：`/ws/dashboard`（在
//! `modules::ws_router()` 下挂 `/ws`，不带 `/api/v2` 前缀，与前端 nginx
//! `/ws/*` → `rust-backend:3000` 反代一致）。
//!
//! 2026-09-15 takeover-fill：完整握手 + 快照推送 + 业务事件订阅 + 30s 心跳。

pub mod dto;
pub mod handler;
pub mod service;

use std::sync::Arc;
use axum::routing::get;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/dashboard", get(handler::ws_dashboard))
}