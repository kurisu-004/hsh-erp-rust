//! process 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/process.py。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut *tx` 给 service → 显式 `tx.commit()`；
//!   提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//!   装进同一个 `R` 信封。
//! - 权限在 service 层（`current.require_any_role` / `require_role`），此处不重复校验。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::process::dto::{
    ProcessCreateRequest, ProcessListOut, ProcessListQuery, ProcessOut, ProcessUpdateRequest,
};
use crate::modules::process::service::ProcessService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/processes
pub async fn list_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<ProcessListQuery>,
) -> Result<Json<R<ProcessListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ProcessService::list_processes(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/processes → 201
pub async fn create_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ProcessCreateRequest>,
) -> Result<(StatusCode, Json<R<ProcessOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ProcessService::create_process(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/processes/{id}
pub async fn get_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<ProcessOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ProcessService::get_process(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/processes/{id}/update
pub async fn update_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<ProcessUpdateRequest>,
) -> Result<Json<R<ProcessOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ProcessService::update_process(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/processes/{id}/soft-delete
pub async fn soft_delete_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    ProcessService::soft_delete_process(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/processes`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_processes).post(create_process))
        .route("/{id}", get(get_process))
        .route("/{id}/update", post(update_process))
        .route("/{id}/soft-delete", post(soft_delete_process))
}