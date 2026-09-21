//! outsource 域 HTTP handler（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/api/v1/outsource_*.py。
//!
//! ## 事务边界（2026-09-22 refactor 对齐 iam 范本）
//! handler 负责 `pool.begin()` / `tx.commit()`——与 iam / 11 个其它 handler 文件现状对齐：
//! - ① **纯写端点**（create_company / update_company / soft_delete_company /
//!   set_company_processes / create_quote / update_quote / submit_quote / approve_quote /
//!   reject_quote / soft_delete_quote / reconcile_update_shipment）：
//!   `pool.begin()` → service call → `tx.commit()`，错误路径 tx drop 隐式回滚。
//! - ② **写 + post-commit 副作用**：outsource 域当前无 Redis / WS 副作用需求，
//!   故全部写端点走形态 ①。
//! - ③ **读端点**（list_companies / get_company / list_companies_by_process /
//!   list_quotes / get_quote）：`pool.acquire()` 不开事务，
//!   service 借 `&mut *conn` 执行查询，用完即 drop。
//!
//! service 形参：`repo: R: OutsourceRepoTrait`（by-value）。生产路径
//! `R = &mut PgConnection`，trait `OutsourceRepoTrait` 已直接对 `&mut PgConnection`
//! 实现（见 `repo/mod.rs`）。`OutsourceService` 字段仅 `Arc<SnowflakeIdGenerator>`，
//! 由 `state.outsource_service` 注入。
//!
//! ## 统一响应信封
//! handler 返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//! 装进同一个 `R` 信封。
//!
//! ## 权限
//! 权限守卫在 service 层（`current.require_any_role`），handler 不重复校验。
//!
//! ## 路由表（17 端点）
//!
//! - `GET    /outsource-companies`              — 列表（READ）
//! - `POST   /outsource-companies`              — 新建（WRITE）
//! - `GET    /outsource-companies/{id}`         — 详情
//! - `POST   /outsource-companies/{id}/update`  — 更新（OCC）
//! - `POST   /outsource-companies/{id}/soft-delete` — 软删
//! - `GET    /outsource-companies/by-process/{process_id}` — 按工序反查
//! - `POST   /outsource-companies/{id}/processes` — 整体替换工序映射
//!
//! - `GET    /outsource-quotes`                 — 列表
//! - `POST   /outsource-quotes`                 — 新建 DRAFT
//! - `GET    /outsource-quotes/{id}`            — 详情
//! - `POST   /outsource-quotes/{id}/update`     — 更新 DRAFT
//! - `POST   /outsource-quotes/{id}/submit`     — DRAFT → SUBMITTED
//! - `POST   /outsource-quotes/{id}/approve`    — SUBMITTED → APPROVED (MANAGER-only)
//! - `POST   /outsource-quotes/{id}/reject`     — SUBMITTED → REJECTED (MANAGER-only)
//! - `POST   /outsource-quotes/{id}/soft-delete` — 软删 DRAFT/REJECTED
//!
//! - `POST   /outsource-shipments/{id}/reconcile-update` — 对账页更新

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::outsource::dto::*;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

// ===========================================================================
//  Company
// ===========================================================================

/// GET /outsource-companies —— 读端点，acquire 不开事务
pub async fn list_companies(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceCompanyListQuery>,
) -> Result<Json<R<OutsourceCompanyListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_companies(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies → 201 —— 纯写端点
pub async fn create_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceCompanyCreateRequest>,
) -> Result<(StatusCode, Json<R<OutsourceCompanyWithProcessesOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .create_company(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /outsource-companies/{id} —— 读端点，acquire 不开事务
pub async fn get_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .get_company(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/update —— 纯写端点
pub async fn update_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceCompanyUpdateRequest>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .update_company(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/soft-delete —— 纯写端点
pub async fn soft_delete_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .outsource_service
        .soft_delete_company(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

/// GET /outsource-companies/by-process/{process_id}
///
/// 静态段必须在 `/{company_id}` catch-all 之前注册。
/// 读端点，acquire 不开事务
pub async fn list_companies_by_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(process_id): Path<i64>,
) -> Result<Json<R<Vec<OutsourceCompanyOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_companies_for_process(&mut *conn, process_id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/processes —— 纯写端点
pub async fn set_company_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetOutsourceCompanyProcessRequest>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .set_company_processes(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

// ===========================================================================
//  Quote
// ===========================================================================

/// GET /outsource-quotes —— 读端点，acquire 不开事务
pub async fn list_quotes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceQuoteListQuery>,
) -> Result<Json<R<OutsourceQuoteListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_quotes(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes → 201 —— 纯写端点
pub async fn create_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceQuoteCreateRequest>,
) -> Result<(StatusCode, Json<R<OutsourceQuoteOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .create_quote(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /outsource-quotes/{id} —— 读端点，acquire 不开事务
pub async fn get_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .get_quote(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/update —— 纯写端点
pub async fn update_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteUpdateRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .update_quote(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/submit —— 纯写端点
pub async fn submit_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .submit_quote(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/approve  (MANAGER-only via service) —— 纯写端点
pub async fn approve_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteApproveRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .approve_quote(&mut *tx, id, req.review_note.as_deref(), req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/reject  (MANAGER-only via service) —— 纯写端点
pub async fn reject_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteRejectRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .reject_quote(&mut *tx, id, &req.review_note, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/soft-delete —— 纯写端点
pub async fn soft_delete_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .outsource_service
        .soft_delete_quote(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

// ===========================================================================
//  Shipment
// ===========================================================================

/// POST /outsource-shipments/{id}/reconcile-update —— 纯写端点
pub async fn reconcile_update_shipment(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceShipmentReconcileUpdateRequest>,
) -> Result<Json<R<OutsourceShipmentOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .reconcile_update_shipment(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

// ===========================================================================
//  Router（注意静态段必须在 catch-all `/{id}` 之前注册）
// ===========================================================================

/// Company 路由（挂载点 `/outsource-companies`）
pub fn company_router() -> Router<Arc<AppState>> {
    Router::new()
        // 静态段必须在 `/{id}` catch-all 之前
        .route("/by-process/{process_id}", get(list_companies_by_process))
        .route("/", get(list_companies).post(create_company))
        .route("/{id}/update", post(update_company))
        .route("/{id}/soft-delete", post(soft_delete_company))
        .route("/{id}/processes", post(set_company_processes))
        .route("/{id}", get(get_company))
}

/// Quote 路由（挂载点 `/outsource-quotes`）
pub fn quote_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_quotes).post(create_quote))
        .route("/{id}/update", post(update_quote))
        .route("/{id}/submit", post(submit_quote))
        .route("/{id}/approve", post(approve_quote))
        .route("/{id}/reject", post(reject_quote))
        .route("/{id}/soft-delete", post(soft_delete_quote))
        .route("/{id}", get(get_quote))
}

/// Shipment 路由（挂载点 `/outsource-shipments`）
pub fn shipment_router() -> Router<Arc<AppState>> {
    Router::new().route("/{id}/reconcile-update", post(reconcile_update_shipment))
}
