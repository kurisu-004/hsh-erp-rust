//! dashboard 域（WebSocket 大屏）
//!
//! 对应 Python myERP/api/v1/ws.py。
//!
//! 端点路径（两类入口）：
//! - `GET /ws/dashboard`（在 `modules::ws_router()` 下挂 `/ws`，不带 `/api/v2` 前缀，
//!   与前端 nginx `/ws/*` → `rust-backend:3000` 反代一致）
//! - `GET /api/v2/dashboard/snapshot`（2026-09-28 新增）：HTTP 全量首取，
//!   与 `/ws/dashboard` 共用 service（前端走「HTTP 首取 + WS 事件 invalidate」模式）
//!
//! ## 2026-09-15 takeover-fill：完整握手 + 快照推送 + 业务事件订阅 + 30s 心跳
//! ## 2026-09-22 Group E 重构：dashboard 域对齐 iam 事务分层范式
//! - 新增 `repo/`（胖 trait `DashboardRepoTrait` + ZST `DashboardRepo` + 4 聚合方法）
//! - 拆 `service/` 为 `service/{mod, snapshot}.rs`（snapshot 子域）
//! - handler 三形态 ①（snapshot 单次只读聚合，开 tx → service → commit）
//! - WS upgrade 拉一次 snapshot，后续 ws_hub.broadcast 是订阅模式不再走 service
//!
//! ## 2026-09-28 新增 HTTP `/snapshot` 端点
//! - `pub fn http_router()` 挂在 `modules::v2_router` 的 `/dashboard` nest 下
//! - 与 WS 端点共用 `DashboardService::build_snapshot_with_workers`（同一 service、
//!   同一 SQL；不引入新 repo 调用）
//! - 鉴权走 `CurrentUser` extractor（任意已登录），与 WS 端点对齐；HTTP 鉴权走
//!   `v2_router` 末尾 `authenticate_middleware`，WS 端点独立 verify_session_token
//! - WS handshake 仍推一次 snapshot（不变，向后兼容老客户端）

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::routing::get;
use axum::Router;
use std::sync::Arc;

/// `/ws/*` 入口（WebSocket）：当前唯一端点 `/ws/dashboard`
///
/// 在 `modules::ws_router()` 下挂 `/ws` 前缀（不带 `/api/v2`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/dashboard", get(handler::ws_dashboard))
}

/// `/api/v2/dashboard/*` HTTP 入口（2026-09-28 新增）
///
/// 当前唯一端点：`GET /snapshot`（HTTP 全量首取大屏快照）。
///
/// 鉴权：依赖 `v2_router` 末尾的 `authenticate_middleware`（Bearer JWT +
/// Redis session），handler 内 `CurrentUser` extractor 仅作类型层签名 + 已
/// 登录语义表达；权限对齐 WS（任意已登录）。
pub fn http_router() -> Router<Arc<AppState>> {
    Router::new().route("/snapshot", get(handler::get_snapshot))
}
