//! worker 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/worker.py。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut *tx` 给 service → 显式 `tx.commit()`；
//!   提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//!   装进同一个 `R` 信封。
//! - 权限在 service 层（`require_role` / `require_auth`），此处不重复校验。
//!
//! ## 7 端点
//! 公开（任意已登录）：`POST /workers/verify-badge`
//! MANAGER-only（6）：`GET /workers` / `POST /workers` / `GET /workers/{id}` /
//!                    `POST /workers/{id}/update` / `POST /workers/{id}/deactivate` /
//!                    `POST /workers/{id}/reactivate`

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::worker::dto::{
    VerifyBadgeRequest, WorkerCreateRequest, WorkerListOut, WorkerListQuery, WorkerOut,
    WorkerUpdateRequest,
};
use crate::modules::worker::service::WorkerService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// POST /api/v2/workers/verify-badge —— 任意已登录用户可调（含 SHELF_ACCOUNT）
pub async fn verify_badge(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<VerifyBadgeRequest>,
) -> Result<Json<R<WorkerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkerService::verify_badge(&mut tx, &req.badge_code, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/workers —— MANAGER-only
pub async fn list_workers(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<WorkerListQuery>,
) -> Result<Json<R<WorkerListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkerService::list_workers(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/workers → 201 —— MANAGER-only
pub async fn create_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<WorkerCreateRequest>,
) -> Result<(StatusCode, Json<R<WorkerOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkerService::create_worker(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/workers/{id} —— MANAGER-only
pub async fn get_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<WorkerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkerService::get_worker(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/workers/{id}/update —— MANAGER-only
pub async fn update_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<WorkerUpdateRequest>,
) -> Result<Json<R<WorkerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkerService::update_worker(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/workers/{id}/deactivate —— MANAGER-only
pub async fn deactivate_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    WorkerService::deactivate_worker(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// POST /api/v2/workers/{id}/reactivate —— MANAGER-only
pub async fn reactivate_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<WorkerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkerService::reactivate_worker(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// 本域路由表（挂载点 `/api/v2/workers`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_workers).post(create_worker))
        .route("/verify-badge", post(verify_badge))
        .route("/{id}", get(get_worker))
        .route("/{id}/update", post(update_worker))
        .route("/{id}/deactivate", post(deactivate_worker))
        .route("/{id}/reactivate", post(reactivate_worker))
}
