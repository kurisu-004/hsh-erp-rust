//! applicant 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/applicant.py（仅 5 个标准 CRUD 端点；
//! `/search` 与 `/bulk-get-or-create` 不在本轮范围）。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut *tx` 给 service → `tx.commit()`；
//!   提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//!   装进同一个 `R` 信封。
//! - 权限在 service 层（`current.require_any_role(...)`），此处不重复校验。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::applicant::dto::{
    ApplicantCreateRequest, ApplicantListOut, ApplicantListQuery, ApplicantOut, ApplicantUpdateRequest,
};
use crate::modules::applicant::service::ApplicantService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/applicants
pub async fn list_applicants(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<ApplicantListQuery>,
) -> Result<Json<R<ApplicantListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ApplicantService::list_applicants(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/applicants → 201
pub async fn create_applicant(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ApplicantCreateRequest>,
) -> Result<(StatusCode, Json<R<ApplicantOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ApplicantService::create_applicant(
        &mut tx, &state.snowflake, &req, &current,
    ).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/applicants/{id}
pub async fn get_applicant(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<ApplicantOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ApplicantService::get_applicant(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/applicants/{id}/update
pub async fn update_applicant(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<ApplicantUpdateRequest>,
) -> Result<Json<R<ApplicantOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ApplicantService::update_applicant(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/applicants/{id}/soft-delete
pub async fn soft_delete_applicant(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    ApplicantService::soft_delete_applicant(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/applicants`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_applicants).post(create_applicant))
        .route("/{id}", get(get_applicant))
        .route("/{id}/update", post(update_applicant))
        .route("/{id}/soft-delete", post(soft_delete_applicant))
}