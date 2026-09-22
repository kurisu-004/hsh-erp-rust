//! statistics HTTP handler（2026-09-15 takeover-fill + 2026-09-23 PR8 重构）
//!
//! 端点（挂在 `/api/v2/statistics`，由 `mod.rs::router()` 桥接）：
//! - `GET /overview`                          —— 生产概览（MANAGER-only）
//! - `GET /workers`                           —— 工人贡献度一览（MANAGER-only）
//! - `GET /workers/{worker_id}`               —— 工人详情（MANAGER-only）
//! - `GET /pickup-skips`                      —— 跳序取件次数汇总（MANAGER-only）
//! - `GET /pickup-skips/{worker_id}`          —— 工人跳序明细（分页；MANAGER-only）
//!
//! 约束：
//! - 事务边界在 handler：
//!   - **只读端点（5/5）**：`pool.acquire()` → 借 `&mut *conn` 给 service → drop conn 释放；
//!     不开事务（2026-09-23 PR8 与 iam 2026-09-21 read-acquire 范式同步）。
//!   - 写端点：本域无写端点。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限：handler 层 `current.require_role(Role::Manager)`（plan §4.1 决议）。
//!
//! ## 2026-09-23 PR8
//! - 全部 5 个端点从 `pool.begin() + tx.commit()` 改为 `pool.acquire()`（statistics 端点全只读）。
//! - service 签名从 `&self, conn: &mut PgConnection` 改为 `&self, repo: &mut conn`（trait 借位）。
//! - trait 已直接 `impl for &mut PgConnection`（与 iam / dashboard / shelf 同形），handler
//!   借 `&mut *conn` 直接喂给 service 即可。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::statistics::dto::{DateRangeQuery, PickupSkipDetailQuery};
use crate::modules::statistics::vo::{
    OverviewOut, PickupSkipDetailOut, PickupSkipSummaryOut, WorkerDetailOut, WorkerStatsListOut,
};
use crate::shared::response::R;
use crate::state::AppState;

/// `GET /api/v2/statistics/overview`
pub async fn overview(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DateRangeQuery>,
) -> Result<Json<R<OverviewOut>>, crate::shared::error::AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = state
        .statistics_service
        .overview(&mut *conn, q.date_from, q.date_to)
        .await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/statistics/workers`
pub async fn workers_stats(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DateRangeQuery>,
) -> Result<Json<R<WorkerStatsListOut>>, crate::shared::error::AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = state
        .statistics_service
        .worker_stats(&mut *conn, q.date_from, q.date_to)
        .await?;
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
    let mut conn = state.pool.acquire().await?;
    let out = state
        .statistics_service
        .worker_detail(&mut *conn, &worker_id, q.date_from, q.date_to)
        .await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/statistics/pickup-skips`
pub async fn pickup_skips(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<PickupSkipSummaryOut>>, crate::shared::error::AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = state
        .statistics_service
        .pickup_skip_summary(&mut *conn)
        .await?;
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
    let mut conn = state.pool.acquire().await?;
    let out = state
        .statistics_service
        .pickup_skip_detail(&mut *conn, &worker_id, limit, offset)
        .await?;
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
