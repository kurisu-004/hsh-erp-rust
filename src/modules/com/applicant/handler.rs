//! applicant 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/applicant.py（仅 5 个标准 CRUD 端点；
//! `/search` 与 `/bulk-get-or-create` 不在本轮范围）。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! handler 负责 `pool.begin()` / `tx.commit()`，按读写分三形态：
//! - ① **纯写端点**（create_applicant / update_applicant / soft_delete_applicant）：
//!   `pool.begin()` → service call → `tx.commit()`，错误路径 tx drop 隐式回滚。
//! - ② **写 + post-commit 副作用**：applicant 域当前无 Redis / WS 副作用需求，
//!   故全部写端点走形态 ①。
//! - ③ **读端点**（list_applicants / get_applicant）：`pool.acquire()` 不开事务，
//!   service 借 `&mut *conn` 执行查询，用完即 drop。
//!
//! service 形参：写端点 `repo: R: ApplicantRepo`（by-value）；
//! 读 list 端点 `repo: R: ApplicantRepo, customer_repo: R3: CustomerRepo`（by-value；
//! 两个 trait 独立单借位，handler 借 `&mut *tx` 喂两次 reborrow）。
//! 生产路径 `R = R3 = &mut PgConnection`，trait 已直接对 `&mut PgConnection`
//! 实现（见 `repo/mod.rs` / `customer/repo/mod.rs`）。
//!
//! ## 统一响应信封
//! handler 返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//! 装进同一个 `R` 信封。
//!
//! ## 权限
//! 权限守卫在 service 层（`require_role` / `require_any_role`），handler 不重复校验。
//!
//! ## 5 端点
//! 读 2：list_applicants / get_applicant
//! 写 3 (MANAGER+CLERK)：create_applicant / update_applicant / soft_delete_applicant

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::com::applicant::dto::{
    ApplicantCreateRequest, ApplicantListOut, ApplicantListQuery, ApplicantOut,
    ApplicantUpdateRequest,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/com/applicants —— 读端点，acquire 不开事务；service 借 `&mut *conn`
/// 同时喂 ApplicantRepo（list_with_filters 等）与 CustomerRepo（lookup_names 批量补 name）。
///
/// 注：list_applicants 形参 `<R: ApplicantRepoTrait + CustomerRepoTrait>` —— 单 R 同时
/// 实现两个 trait（生产 `R = &mut PgConnection`，本身两个 trait 都对 `&mut PgConnection`
/// 实现），handler 借 `&mut *conn` 一次即可，避免 rustc E0499（rust 2024 borrow checker
/// 不允许对同一连接做两次 `&mut *`）。
pub async fn list_applicants(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<ApplicantListQuery>,
) -> Result<Json<R<ApplicantListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .applicant_service
        .list_applicants(&mut conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/com/applicants → 201 —— 纯写端点
pub async fn create_applicant(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ApplicantCreateRequest>,
) -> Result<(StatusCode, Json<R<ApplicantOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .applicant_service
        .create_applicant(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/com/applicants/{id} —— 读端点，acquire 不开事务
pub async fn get_applicant(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<ApplicantOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .applicant_service
        .get_applicant(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/com/applicants/{id}/update —— 纯写端点
pub async fn update_applicant(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<ApplicantUpdateRequest>,
) -> Result<Json<R<ApplicantOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .applicant_service
        .update_applicant(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/com/applicants/{id}/soft-delete —— 纯写端点
pub async fn soft_delete_applicant(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .applicant_service
        .soft_delete_applicant(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/com/applicants`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_applicants).post(create_applicant))
        .route("/{id}", get(get_applicant))
        .route("/{id}/update", post(update_applicant))
        .route("/{id}/soft-delete", post(soft_delete_applicant))
}
