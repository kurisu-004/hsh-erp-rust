//! statistics HTTP handler（2026-09-15 takeover-fill）
//!
//! 端点（挂在 `/api/v2/statistics`，由 `mod.rs::router()` 桥接）：
//! - `GET /overview`                          —— 生产概览（MANAGER-only）
//! - `GET /workers`                           —— 工人贡献度一览（MANAGER-only）
//! - `GET /workers/{worker_id}`               —— 工人详情（MANAGER-only）
//! - `GET /pickup-skips`                      —— 跳序取件次数汇总（MANAGER-only）
//! - `GET /pickup-skips/{worker_id}`          —— 工人跳序明细（分页；MANAGER-only）
//!
//! 约束：
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限：handler 层 `current.require_role(Role::Manager)`（plan §4.1 决议）。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::statistics::dto::{
    DateRangeQuery, OverviewOut, PickupSkipDetailOut, PickupSkipDetailQuery, PickupSkipSummaryOut,
    WorkerDetailOut, WorkerStatsListOut,
};
use crate::modules::statistics::service::StatisticsService;
use crate::shared::response::R;
use crate::state::AppState;

/// `GET /api/v2/statistics/overview`
pub async fn overview(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DateRangeQuery>,
) -> Result<Json<R<OverviewOut>>, crate::shared::error::AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let out = StatisticsService::overview(&mut tx, q.date_from, q.date_to).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/statistics/workers`
pub async fn workers_stats(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DateRangeQuery>,
) -> Result<Json<R<WorkerStatsListOut>>, crate::shared::error::AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let out = StatisticsService::worker_stats(&mut tx, q.date_from, q.date_to).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/statistics/workers/{worker_id}`
pub async fn worker_detail(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(worker_id): Path<String>,
    Query(q): Query<DateRangeQuery>,
) -> Result<Json<R<WorkerDetailOut>>, crate::shared::error::AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let out = StatisticsService::worker_detail(&mut tx, &worker_id, q.date_from, q.date_to).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/statistics/pickup-skips`
pub async fn pickup_skips(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<PickupSkipSummaryOut>>, crate::shared::error::AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let out = StatisticsService::pickup_skip_summary(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/statistics/pickup-skips/{worker_id}`
pub async fn pickup_skip_detail(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(worker_id): Path<String>,
    Query(q): Query<PickupSkipDetailQuery>,
) -> Result<Json<R<PickupSkipDetailOut>>, crate::shared::error::AppError> {
    current.require_role(Role::Manager)?;
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let mut tx = state.pool.begin().await?;
    let out = StatisticsService::pickup_skip_detail(&mut tx, &worker_id, limit, offset).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/overview", get(overview))
        .route("/workers", get(workers_stats))
        .route("/workers/{worker_id}", get(worker_detail))
        .route("/pickup-skips", get(pickup_skips))
        .route("/pickup-skips/{worker_id}", get(pickup_skip_detail))
}
