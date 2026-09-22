//! dashboard 域（WebSocket 大屏）
//!
//! 对应 Python myERP/api/v1/ws.py。端点路径：`/ws/dashboard`（在
//! `modules::ws_router()` 下挂 `/ws`，不带 `/api/v2` 前缀，与前端 nginx
//! `/ws/*` → `rust-backend:3000` 反代一致）。
//!
//! ## 2026-09-15 takeover-fill：完整握手 + 快照推送 + 业务事件订阅 + 30s 心跳
//! ## 2026-09-22 Group E 重构：dashboard 域对齐 iam 事务分层范式
//! - 新增 `repo/`（胖 trait `DashboardRepoTrait` + ZST `DashboardRepo` + 4 聚合方法）
//! - 拆 `service/` 为 `service/{mod, snapshot}.rs`（snapshot 子域）
//! - handler 三形态 ①（snapshot 单次只读聚合，开 tx → service → commit）
//! - WS upgrade 拉一次 snapshot，后续 ws_hub.broadcast 是订阅模式不再走 service

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use axum::routing::get;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/dashboard", get(handler::ws_dashboard))
}