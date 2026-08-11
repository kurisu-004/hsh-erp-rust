//! dashboard 域（WebSocket 大屏）
//!
// 对应 Python myERP/api/v1/ws.py。端点路径：`/ws/dashboard`。

pub mod dto;
pub mod handler;
pub mod service;

use std::sync::Arc;
use axum::routing::get;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/dashboard", get(ws_handler_stub))
}

async fn ws_handler_stub() {
    // 实施阶段补：WebSocketUpgrade 握手 + JWT 校验 + 双向 spawn 任务
}
