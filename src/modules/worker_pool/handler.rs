//! worker_pool 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/worker_pool.py（设计 §6.3 — worker-pool）。
//!
//! ## 端点
//! - `GET  /api/v2/worker-pool/state?worker_id=&shelf_id=`  —— worker 当前持有 +
//!   池候选数（按工序分组）。无 role guard（worker 自查 + admin 监控共用）。
//! - `POST /api/v2/admin/worker-pool/refill`                —— admin 触发
//!   `refill_for_worker`。Manager role 守卫。
//! - `POST /api/v2/admin/worker-pool/remove`                —— admin 把 worker
//!   持有的批次按 RETURNED 语义放回候选池。Manager role 守卫。
//!
//! ## 事务 + WS 广播
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//!   `tx.commit()`；提前 return 时 `Transaction` 的 Drop 自动回滚。
//! - WS 广播在 commit 之后（Python `session.info` 延迟模式对齐）。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{AdminRefillRequest, AdminRemoveRequest};
use super::model::RefillResult;
use super::model::WorkerPoolState;
use super::service::WorkerPoolService;

#[derive(Debug, Deserialize)]
pub struct StateQuery {
    pub worker_id: i64,
    pub shelf_id: i64,
}

/// GET /api/v2/worker-pool/state?worker_id=&shelf_id=
///
/// 无 role guard —— worker 自查 / admin 监控共用。
pub async fn state(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StateQuery>,
) -> Result<Json<R<WorkerPoolState>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let s = WorkerPoolService::compute_state(&mut tx, q.worker_id, q.shelf_id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(s)))
}

/// POST /api/v2/admin/worker-pool/refill
///
/// Manager role 守卫。Commit 后：
/// - `taken.len() > 0` → 广播 `WORKER_POOL_REFILL_DONE`
/// - `pool_empty`（没抢到任何一批） → 广播 `WORKER_POOL_EMPTY`
pub async fn admin_refill(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<AdminRefillRequest>,
) -> Result<Json<R<RefillResult>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let r = WorkerPoolService::refill_for_worker(
        &mut tx,
        &state.snowflake,
        req.worker_id,
        req.shelf_id,
        current.id,
    )
    .await?;
    tx.commit().await?;
    if !r.taken.is_empty() {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "WORKER_POOL_REFILL_DONE".into(),
            payload: serde_json::to_value(&r).unwrap_or_default(),
        });
    } else if r.pool_empty {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "WORKER_POOL_EMPTY".into(),
            payload: serde_json::json!({
                "worker_id": req.worker_id.to_string(),
                "shelf_id": req.shelf_id.to_string(),
            }),
        });
    }
    Ok(Json(R::ok(r)))
}

/// POST /api/v2/admin/worker-pool/remove
///
/// Manager role 守卫。把 worker 持有的指定 batch 按 RETURNED 语义放回候选池。
/// Commit 后广播 `WORKER_POOL_ADMIN_REMOVED`。
pub async fn admin_remove(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<AdminRemoveRequest>,
) -> Result<Json<R<super::model::TakenItem>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let t = WorkerPoolService::admin_remove_held_batch(&mut tx, &state.snowflake, req, &current)
        .await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "WORKER_POOL_ADMIN_REMOVED".into(),
        payload: serde_json::to_value(&t).unwrap_or_default(),
    });
    Ok(Json(R::ok(t)))
}
