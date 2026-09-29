//! prod::batch 子模块 —— PENDING 批次列表 + 下发端点
//!
//! 2026-09-29 新增：prod 域内承担车间「PENDING 待下发批次列表 + 一次 / 多次 /
//! 自动下发」4 端点。URL 全部挂 `/api/v2/prod/batches/*`。
//!
//! ## 模块结构（与 worker_pool / process_chain 平行）
//! - `dto.rs` —— 入参（`DispatchRequest` / `BulkDispatchRequest` /
//!   `AutoDispatchRequest` / 列表 Query）
//! - `vo.rs` —— 出参（`PendingBatchListOut` / `PendingBatchItem` /
//!   `DispatchResult` / `BulkDispatchResult` / `AutoDispatchResult`）
//! - `repo.rs` —— SQL 真源（`list_pending_batches` / `find_batch_by_id` /
//!   `find_first_shelf_for_process` / `update_batch_dispatched` /
//!   `first_step_of_chain` / `part_get_process_chain_id`）
//! - `service.rs` —— 业务逻辑（`list_pending` / `dispatch_batch` 内部 helper +
//!   `bulk_dispatch` / `auto_dispatch`），事务边界下沉到 handler
//! - `handler.rs` —— HTTP 路由 + 角色守卫 + WS 广播
//!
//! ## 货架解析
//! 沿用车间 `t_shelf_process` 表（已存在，零 schema 变更），按
//! `target_process_id` 取 `LIMIT 1` 解析货架；多结果取 sort_order 最小者
//! （`ORDER BY sort_order ASC, id ASC` 兜底）。
//!
//! ## 事务 / WS 广播
//! 写端点（dispatch / bulk / auto）：handler `state.pool.begin()` →
//! service → `tx.commit()` → WS `BATCH_PLACED_ON_SHELF` 广播。读端点
//! （list_pending）走 `pool.acquire()` 不开事务。
//!
//! ## 角色守卫
//! - GET pending: Manager + Clerk + Inspector
//! - POST dispatch / bulk / auto: Manager + Clerk

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/pending", get(handler::list_pending))
        .route("/dispatch", post(handler::dispatch))
        .route("/bulk-dispatch", post(handler::bulk_dispatch))
        .route("/auto-dispatch", post(handler::auto_dispatch))
}
