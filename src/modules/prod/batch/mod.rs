//! prod::batch 子模块 —— PENDING 批次列表 + 下发端点
//!
//! 2026-09-29 新增 + 2026-09-30 重构：
//! - `dispatch` 统一 bulk-only（单条下发即 `targets.length == 1`）
//! - `auto-dispatch` 改为只读查询（返回首道工序 + 首货架）
//! - `bulk-dispatch` 端点删除（路由层不再挂载）
//!
//! URL 全部挂 `/api/v2/prod/batches/*`。
//!
//! ## 模块结构（与 worker_pool / process_chain 平行）
//! - `dto.rs` —— 入参（`DispatchRequest` / `AutoDispatchRequest` / `ListPendingQuery`）
//! - `vo.rs` —— 出参（`PendingBatchListOut` / `PendingBatchItem` / `DispatchResult`
//!   / `AutoDispatchResult` / `AutoDispatchItem`）
//! - `repo.rs` —— SQL 真源（`list_pending_batches` / `find_batch_by_id` /
//!   `update_batch_dispatched` / `first_step_of_chain` /
//!   `part_get_process_chain_id` / `preview_auto_dispatch` /
//!   `find_part_id_by_batch_id`）
//! - `service.rs` —— 业务逻辑（`list_pending` / `dispatch_batch` bulk-only /
//!   `auto_dispatch_preview`），事务边界下沉到 handler
//! - `handler.rs` —— HTTP 路由 + 角色守卫 + WS 广播
//!
//! ## 货架解析
//! 沿用车间 `t_shelf_process` 表（已存在，零 schema 变更），按
//! `target_process_id` 取 `LIMIT 1` 解析货架；多结果取 sort_order 最小者
//! （`ORDER BY sort_order ASC, id ASC` 兜底）。
//!
//! 2026-10-02：`t_shelf_process` 的 SQL 真源已归 `prod::shelf_process::repo`
//! （`ShelfProcessRepo::find_first_shelf_for_process`），本域 dispatch 路径改调该处；
//! `preview_auto_dispatch` 因是 `LEFT JOIN LATERAL` 大复合查询**保留 inline**
//! （见 `repo.rs` 同名函数 doc）。
//!
//! ## 事务 / WS 广播
//! - 写端点（dispatch）：handler `state.pool.begin()` → service →
//!   `tx.commit()` → WS `BATCH_PLACED_ON_SHELF` 广播
//! - 读端点（pending）：handler `pool.acquire()` 不开事务
//! - 只读端点（auto-dispatch）：handler `pool.acquire()` 不开事务，**不发** WS 广播
//!
//! ## 角色守卫
//! - GET pending: Manager + Clerk + Inspector
//! - POST dispatch / auto-dispatch: Manager + Clerk

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
        // 2026-09-30 重构：dispatch 统一 bulk-only（单条下发即 targets.length==1）
        .route("/dispatch", post(handler::dispatch))
        // 2026-09-30 重构：auto-dispatch 改为只读查询
        .route("/auto-dispatch", post(handler::auto_dispatch))
}
