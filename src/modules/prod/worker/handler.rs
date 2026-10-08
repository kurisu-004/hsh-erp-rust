//! worker 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/worker.py。
//!
//! ## 约定
//! - 事务边界在 handler（2026-09-22 D-2-simple 重构后）：handler 显式
//!   `state.pool.begin()` / `tx.commit()`，service 仅业务逻辑。
//! - service 形参：`repo: R: WorkerRepoTrait`（by-value）。生产路径
//!   `R = &mut PgConnection`，trait `WorkerRepoTrait` 已直接对 `&mut PgConnection`
//!   实现（见 `repo/mod.rs`）；handler 借 `&mut *tx` 喂给 service。
//! - 统一响应信封：返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//!   装进同一个 `R` 信封。
//! - 权限在 service 层（`require_role` / `require_auth`），此处不重复校验。
//!
//! ## 6 端点（全部 MANAGER-only）
//! `GET /workers` / `POST /workers` / `GET /workers/{id}` /
//! `POST /workers/{id}/update` / `POST /workers/{id}/deactivate` /
//! `POST /workers/{id}/reactivate`
//!
//! 2026-10-10：报工台的 `POST /workers/verify-badge`（任意已登录可调）连同其
//! service 方法与 DTO 迁往 `crate::modules::prod::scan`（新路径
//! `POST /api/v2/prod/scan/verify-badge`，**无 alias**）—— 按「目标域按前端消费方
//! 判定」的规约，它属于报工台而不属于工人档案。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::prod::worker::dto::{
    WorkerCreateRequest, WorkerListQuery, WorkerUpdateRequest,
};
use crate::modules::prod::worker::vo::{WorkerListOut, WorkerOut};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/prod/workers —— MANAGER-only
pub async fn list_workers(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<WorkerListQuery>,
) -> Result<Json<R<WorkerListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .worker_service
        .list_workers(&mut *tx, &query, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/workers → 201 —— MANAGER-only
pub async fn create_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<WorkerCreateRequest>,
) -> Result<(StatusCode, Json<R<WorkerOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .worker_service
        .create_worker(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/prod/workers/{id} —— MANAGER-only
pub async fn get_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<WorkerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .worker_service
        .get_worker(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/workers/{id}/update —— MANAGER-only
pub async fn update_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<WorkerUpdateRequest>,
) -> Result<Json<R<WorkerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .worker_service
        .update_worker(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/workers/{id}/deactivate —— MANAGER-only
pub async fn deactivate_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .worker_service
        .deactivate_worker(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// POST /api/v2/prod/workers/{id}/reactivate —— MANAGER-only
pub async fn reactivate_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<WorkerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .worker_service
        .reactivate_worker(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// 本域路由表（挂载点 `/api/v2/prod/workers`，见 `modules::prod::router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_workers).post(create_worker))
        .route("/{id}", get(get_worker))
        .route("/{id}/update", post(update_worker))
        .route("/{id}/deactivate", post(deactivate_worker))
        .route("/{id}/reactivate", post(reactivate_worker))
}
