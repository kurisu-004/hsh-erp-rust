//! prod::queue 的**队列写端点** HTTP handler（refill / move / auto-allocate）
//!
//! 2026-10-08 自原 `handler.rs` 拆出（该文件同时含 3 个读端点，聚合读已迁到
//! `handler/board.rs`）。
//!
//! ## 端点
//! - `POST /api/v2/prod/queue/refill` —— Manager 触发 `refill_for_worker`。
//! - `POST /api/v2/prod/queue/move` —— Manager 通用移动端点（POOL ↔ WORKER +
//!   WORKER ↔ WORKER 三方向，取代旧 `admin_remove` / `admin_assign`）。commit 后
//!   广播 `WORKER_POOL_MOVE_DONE`。
//! - `POST /api/v2/prod/queue/auto-allocate` —— 按 `process_id + shelf_id` 范围
//!   自动为每个匹配 worker 抢批次数 / 累计工时。commit 后广播
//!   `WORKER_POOL_AUTO_ALLOCATE_DONE`。
//!
//! ## 事务 + WS 广播
//! 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//! `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! 三个端点都是「pool.begin() → service → commit」，**广播在 commit 之后**。
//!
//! service 公共方法收 `&mut PgConnection`（生产），service 内部 reborrow
//! `&mut *conn` 喂 `QueueRepoTrait`（trait 已直接 `impl for &mut PgConnection`）。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use crate::modules::prod::queue::dto::{AdminRefillRequest, AutoAllocateRequest, MoveRequest};
use crate::modules::prod::queue::service::queue::QueueService;
use crate::modules::prod::queue::vo::worker::{AutoAllocateResult, MoveResult, RefillResult};

/// POST /api/v2/prod/queue/refill
///
/// Manager role 守卫。Commit 后：
/// - `taken.len() > 0` → 广播 `WORKER_POOL_REFILL_DONE`
/// - `pool_empty`（没抢到任何一批） → 广播 `WORKER_POOL_EMPTY`
///
/// 纯写端点（① 形态）：`pool.begin() → service → commit`。
pub async fn admin_refill(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<AdminRefillRequest>,
) -> Result<Json<R<RefillResult>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let r = QueueService::refill_for_worker(
        &mut tx,
        &state.snowflake,
        req.worker_id,
        req.shelf_id,
        current.id,
        &current,
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
            payload: json!({
                "worker_id": req.worker_id.to_string(),
                "shelf_id": req.shelf_id.to_string(),
                "pool_empty": true,
            }),
        });
    }
    Ok(Json(R::ok(r)))
}

/// POST /api/v2/prod/queue/auto-allocate
///
/// 按 `process_id + shelf_id` 范围自动为每个匹配 worker 抢批次数 / 累计工时。
///
/// Manager 角色守卫下沉到 service（`auto_allocate_for_process` 内部 `require_role`）。
/// Commit 后广播 `WORKER_POOL_AUTO_ALLOCATE_DONE`（payload = `AutoAllocateResult`）。
///
/// 纯写端点（① 形态）：`pool.begin() → service → commit`。
pub async fn auto_allocate(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<AutoAllocateRequest>,
) -> Result<Json<R<AutoAllocateResult>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let result =
        QueueService::auto_allocate_for_process(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "WORKER_POOL_AUTO_ALLOCATE_DONE".into(),
        payload: serde_json::to_value(&result).unwrap_or_default(),
    });
    Ok(Json(R::ok(result)))
}

/// POST /api/v2/prod/queue/move（2026-09-30 新增）。
///
/// 通用移动端点：覆盖 POOL ↔ WORKER + WORKER ↔ WORKER 三方向。
/// 取代原 `admin_remove`（WORKER→POOL 单边）+ `admin_assign`（POOL→WORKER 单边）。
///
/// Manager 角色守卫下沉到 service（`move_batch` 内部 `require_role`）。
/// Commit 后统一广播 `WORKER_POOL_MOVE_DONE`（payload 含 from/to 让前端推断方向）。
///
/// 纯写端点（① 形态）：`pool.begin() → service → commit`。
pub async fn move_batch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<MoveRequest>,
) -> Result<Json<R<MoveResult>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let result = QueueService::move_batch(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "WORKER_POOL_MOVE_DONE".into(),
        payload: serde_json::to_value(&result).unwrap_or_default(),
    });
    Ok(Json(R::ok(result)))
}
