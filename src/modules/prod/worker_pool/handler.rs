//! worker_pool 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/worker_pool.py（设计 §6.3 — worker-pool）。
//!
//! ## 端点（2026-09-30 重构：worker-pool → pool 路径收敛 + 5 个端点挂 pool/*）
//! - `GET  /api/v2/prod/pool/state?worker_id=&shelf_id=` —— worker 当前持有 +
//!   池候选数（按工序分组）。无 role guard。
//! - `GET  /api/v2/prod/pool/counts`               —— 2026-09-30 新增：全工序
//!   候选批次聚合计数（GROUP BY process_id），跨所有货架，admin 视图。
//!   Manager+Clerk+Inspector 可调；service 内守卫。
//! - `GET  /api/v2/prod/pool/{process_id}`        —— 按工序返回候选池详情。
//!   Manager+Clerk+Inspector 可调；service 内守卫。
//! - `POST /api/v2/prod/pool/refill`              —— Manager 触发
//!   `refill_for_worker`。Manager role 守卫。
//! - `POST /api/v2/prod/pool/move`                —— Manager 通用移动端点
//!   （覆盖 POOL ↔ WORKER + WORKER ↔ WORKER 三方向，取代旧 `admin_remove` /
//!   `admin_assign`）。Manager role 守卫。Commit 后广播 `WORKER_POOL_MOVE_DONE`。
//! - `POST /api/v2/prod/pool/auto-allocate`       —— 按 `process_id + shelf_id`
//!   范围自动为每个匹配 worker 抢批次数 / 累计工时。Manager role 守卫。
//!
//! ## 事务 + WS 广播（2026-09-22 D-2 重构对齐 iam 范本）
//! 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//! `tx.commit()`；提前 return 时 `Transaction` 的 Drop 自动回滚。读端点走
//! `pool.acquire()` 不开事务。
//!
//! - ① 纯写端点（admin_refill / move / auto_allocate）：
//!   `pool.begin() → service → commit`，commit 后发 WS 广播。
//! - ③ 读端点（state / pool_by_process / pool_counts）：`pool.acquire() → service`，
//!   不开事务。
//!
//! service 公共方法收 `&mut PgConnection`（生产），service 内部 reborrow `&mut *conn`
//! 喂 `WorkerPoolRepoTrait`（trait 已直接 `impl for &mut PgConnection`，2026-09-22
//! 替代任何 `PgWorkerPoolRepo<'a>` 壳）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{AdminRefillRequest, AutoAllocateRequest, MoveRequest, WorkerPoolCountsOut};
use super::model::RefillResult;
use super::model::WorkerPoolState;
use super::service::WorkerPoolService;
use super::vo::{AutoAllocateResult, MoveResult, ProcessPoolDetail};

#[derive(Debug, Deserialize)]
pub struct StateQuery {
    pub worker_id: i64,
    pub shelf_id: i64,
}

/// GET /api/v2/prod/pool/state?worker_id=&shelf_id=
///
/// 无 role guard —— worker 自查 / admin 监控共用。
///
/// 读端点（③ 形态）：`pool.acquire()` 不开事务；service 借 `&mut PgConnection` 跑查询。
pub async fn state(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StateQuery>,
) -> Result<Json<R<WorkerPoolState>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let s = WorkerPoolService::compute_state(&mut conn, q.worker_id, q.shelf_id).await?;
    Ok(Json(R::ok(s)))
}

/// POST /api/v2/prod/pool/refill
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
    let r = WorkerPoolService::refill_for_worker(
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

/// GET /api/v2/prod/pool/{process_id}
///
/// Manager + Clerk + Inspector。返回 process 元数据 + 可执行该工序的工人 +
/// 映射工种的 max_held + 跨生产货架的候选批次全量列表。
///
/// 角色守卫下沉到 service（`pool_by_process` 内部 `require_any_role`），handler
/// 不重复校验（与 work_type/assembly 域惯例一致）。
///
/// 读端点（③ 形态）：`pool.acquire()` 不开事务。
pub async fn pool_by_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    axum::extract::Path(process_id): axum::extract::Path<i64>,
) -> Result<Json<R<ProcessPoolDetail>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let detail = WorkerPoolService::pool_by_process(&mut conn, &current, process_id).await?;
    Ok(Json(R::ok(detail)))
}

/// GET /api/v2/prod/pool/counts
///
/// 2026-09-30 新增：admin 视角的全工序候选批次聚合（dashboard 快照型查询）。
/// 返回 `WorkerPoolCountsOut { counts: Vec<ProcessBatchCount>, total: i64 }`，
/// 跨所有生产货架（不指定 shelf_id，按现有 per-process 端点惯例）。
///
/// Manager + Clerk + Inspector（admin 视角但不止 Manager）；service 内守卫。
///
/// 不发 WS 广播（counts 是 dashboard 快照型查询，无业务流转）。
///
/// 读端点（③ 形态）：`pool.acquire()` 不开事务。
pub async fn pool_counts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<WorkerPoolCountsOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = WorkerPoolService::pool_counts_all_shelves(&mut conn, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/pool/auto-allocate
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
        WorkerPoolService::auto_allocate_for_process(&mut tx, &state.snowflake, req, &current)
            .await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "WORKER_POOL_AUTO_ALLOCATE_DONE".into(),
        payload: serde_json::to_value(&result).unwrap_or_default(),
    });
    Ok(Json(R::ok(result)))
}

/// POST /api/v2/prod/pool/move（2026-09-30 新增）。
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
    let result = WorkerPoolService::move_batch(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "WORKER_POOL_MOVE_DONE".into(),
        payload: serde_json::to_value(&result).unwrap_or_default(),
    });
    Ok(Json(R::ok(result)))
}
