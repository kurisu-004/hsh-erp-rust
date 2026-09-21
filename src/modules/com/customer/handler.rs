//! customer 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/customer.py。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! handler 负责 `pool.begin()` / `tx.commit()`，按读写分三形态：
//! - ① **纯写端点**（create_customer / update_customer / soft_delete_customer）：
//!   `pool.begin()` → service call → `tx.commit()`，错误路径 tx drop 隐式回滚。
//! - ② **写 + post-commit 副作用**：customer 域当前无 Redis / WS 副作用需求，
//!   故全部写端点走形态 ①。
//! - ③ **读端点**（list_customers / get_customer）：`pool.acquire()` 不开事务，
//!   service 借 `&mut *conn` 执行查询，用完即 drop。
//!
//! service 形参：`repo: R: CustomerRepo`（by-value）。生产路径
//! `R = &mut PgConnection`，trait `CustomerRepo` 已直接对 `&mut PgConnection`
//! 实现（见 `repo/mod.rs`）。
//!
//! ## 统一响应信封
//! handler 返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//! 装进同一个 `R` 信封。
//!
//! ## 权限
//! 权限守卫在 service 层（`current.require_any_role`），handler 不重复校验。
//!
//! ## 5 端点
//! 读 2：list_customers / get_customer
//! 写 3 (MANAGER+CLERK)：create_customer / update_customer / soft_delete_customer

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::com::customer::dto::{
    CustomerCreateRequest, CustomerListOut, CustomerListQuery, CustomerOut, CustomerUpdateRequest,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/com/customers —— 读端点，acquire 不开事务
pub async fn list_customers(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<CustomerListQuery>,
) -> Result<Json<R<CustomerListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .customer_service
        .list_customers(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/com/customers → 201 —— 纯写端点
pub async fn create_customer(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<CustomerCreateRequest>,
) -> Result<(StatusCode, Json<R<CustomerOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .customer_service
        .create_customer(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/com/customers/{id} —— 读端点，acquire 不开事务
pub async fn get_customer(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<CustomerOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .customer_service
        .get_customer(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/com/customers/{id}/update —— 纯写端点
pub async fn update_customer(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<CustomerUpdateRequest>,
) -> Result<Json<R<CustomerOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .customer_service
        .update_customer(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/com/customers/{id}/soft-delete —— 纯写端点
pub async fn soft_delete_customer(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .customer_service
        .soft_delete_customer(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/com/customers`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_customers).post(create_customer))
        .route("/{id}", get(get_customer))
        .route("/{id}/update", post(update_customer))
        .route("/{id}/soft-delete", post(soft_delete_customer))
}
