//! customer 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/customer.py。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut *tx` 给 service → 显式 `tx.commit()`；
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
use crate::modules::customer::dto::{
    CustomerCreateRequest, CustomerListOut, CustomerListQuery, CustomerOut, CustomerUpdateRequest,
};
use crate::modules::customer::service::CustomerService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/customers
pub async fn list_customers(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<CustomerListQuery>,
) -> Result<Json<R<CustomerListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = CustomerService::list_customers(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/customers → 201
pub async fn create_customer(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<CustomerCreateRequest>,
) -> Result<(StatusCode, Json<R<CustomerOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = CustomerService::create_customer(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/customers/{id}
pub async fn get_customer(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<CustomerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = CustomerService::get_customer(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/customers/{id}/update
pub async fn update_customer(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<CustomerUpdateRequest>,
) -> Result<Json<R<CustomerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = CustomerService::update_customer(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/customers/{id}/soft-delete
pub async fn soft_delete_customer(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    CustomerService::soft_delete_customer(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/customers`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_customers).post(create_customer))
        .route("/{id}", get(get_customer))
        .route("/{id}/update", post(update_customer))
        .route("/{id}/soft-delete", post(soft_delete_customer))
}