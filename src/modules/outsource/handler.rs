//! outsource 域 HTTP handler（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/api/v1/outsource_*.py。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`
//! - 权限在 handler（`current.require_any_role`）；业务层 service 也会再校验一次
//!
//! ## 路由表（详见 `router()`）
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
use crate::modules::outsource::service::OutsourceService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

// ===========================================================================
//  Company
// ===========================================================================

/// GET /outsource-companies
pub async fn list_companies(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceCompanyListQuery>,
) -> Result<Json<R<OutsourceCompanyListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::list_companies(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies → 201
pub async fn create_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceCompanyCreateRequest>,
) -> Result<(StatusCode, Json<R<OutsourceCompanyWithProcessesOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::create_company(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /outsource-companies/{id}
pub async fn get_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::get_company(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/update
pub async fn update_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceCompanyUpdateRequest>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::update_company(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/soft-delete
pub async fn soft_delete_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    OutsourceService::soft_delete_company(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

/// GET /outsource-companies/by-process/{process_id}
///
/// 静态段必须在 `/{company_id}` catch-all 之前注册。
pub async fn list_companies_by_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(process_id): Path<i64>,
) -> Result<Json<R<Vec<OutsourceCompanyOut>>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::list_companies_for_process(&mut tx, process_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/processes
pub async fn set_company_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetOutsourceCompanyProcessRequest>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::set_company_processes(&mut tx, &state.snowflake, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

// ===========================================================================
//  Quote
// ===========================================================================

/// GET /outsource-quotes
pub async fn list_quotes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceQuoteListQuery>,
) -> Result<Json<R<OutsourceQuoteListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::list_quotes(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes → 201
pub async fn create_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceQuoteCreateRequest>,
) -> Result<(StatusCode, Json<R<OutsourceQuoteOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::create_quote(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /outsource-quotes/{id}
pub async fn get_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::get_quote(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/update
pub async fn update_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteUpdateRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::update_quote(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/submit
pub async fn submit_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::submit_quote(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/approve  (MANAGER-only via service)
pub async fn approve_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteApproveRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::approve_quote(&mut tx, id, req.review_note.as_deref(), req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/reject  (MANAGER-only via service)
pub async fn reject_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteRejectRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::reject_quote(&mut tx, id, &req.review_note, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/soft-delete
pub async fn soft_delete_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    OutsourceService::soft_delete_quote(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

// ===========================================================================
//  Shipment
// ===========================================================================

/// POST /outsource-shipments/{id}/reconcile-update
pub async fn reconcile_update_shipment(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceShipmentReconcileUpdateRequest>,
) -> Result<Json<R<OutsourceShipmentOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = OutsourceService::reconcile_update_shipment(&mut tx, id, &req, &current).await?;
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
        .route(
            "/by-process/{process_id}",
            get(list_companies_by_process),
        )
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
    Router::new()
        .route("/{id}/reconcile-update", post(reconcile_update_shipment))
}
