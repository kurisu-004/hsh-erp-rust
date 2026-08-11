//! user 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/user.py。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut *tx` 给 service → 显式 `tx.commit()`；
//!   提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//!   装进同一个 `R` 信封，不做 middleware 后置包装。
//! - 权限在服务层（`current.require_role(Role::Manager)?`），此处不重复校验：
//!   Python 是 router 级 `dependencies=[require_role(MANAGER)]`，本实现下沉到 service，
//!   保证绕过 HTTP 直接调 service 时同样受控。
//! - 只读接口同样开事务，以获得一致性快照（列表 + count 两条查询之间不会被并发写撕裂）。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use axum::routing::{get, post};

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{
    UserAddRoleRequest, UserCreateRequest, UserListOut, UserListQuery, UserOut, UserRoleOut,
    UserUpdateRequest,
};
use super::service::UserService;

/// GET /api/v2/users
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<UserListQuery>,
) -> Result<Json<R<UserListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = UserService::list_users(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users → 201
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<UserCreateRequest>,
) -> Result<(StatusCode, Json<R<UserOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = UserService::create_user(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/users/{id}
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = UserService::get_user(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users/{id}/update
pub async fn update_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserUpdateRequest>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = UserService::update_user(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users/{id}/reset-password
pub async fn admin_reset_password(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = UserService::admin_reset_password(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users/{id}/deactivate
pub async fn deactivate_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = UserService::deactivate_user(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/users/{id}/roles
pub async fn list_user_roles(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<Vec<UserRoleOut>>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = UserService::list_user_roles(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users/{id}/roles → 201
pub async fn add_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserAddRoleRequest>,
) -> Result<(StatusCode, Json<R<UserRoleOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = UserService::add_role(&mut tx, &state.snowflake, id, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// POST /api/v2/users/{id}/roles/{role_id}/remove
pub async fn remove_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path((id, role_id)): Path<(i64, i64)>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    UserService::remove_role(&mut tx, id, role_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

/// 本域路由表（挂载点 `/api/v2/users`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_users).post(create_user))
        .route("/{id}", get(get_user))
        .route("/{id}/update", post(update_user))
        .route("/{id}/reset-password", post(admin_reset_password))
        .route("/{id}/deactivate", post(deactivate_user))
        .route("/{id}/roles", get(list_user_roles).post(add_role))
        .route("/{id}/roles/{role_id}/remove", post(remove_role))
}
