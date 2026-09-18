//! user 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/user.py。
//!
//! ## 约定（2026-09-18 重构）
//! - 事务边界在 service（见 `service.rs` 头注释）。Handler 完全薄壳化——
//!   9 端点不再直接开/关 sqlx 事务，直接转发到 `state.user_service.xxx(...)`。
//! - 统一响应信封：返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//!   装进同一个 `R` 信封，不做 middleware 后置包装。
//! - 权限在服务层（`current.require_role(Role::Manager)?`），此处不重复校验：
//!   Python 是 router 级 `dependencies=[require_role(MANAGER)]`，本实现下沉到 service，
//!   保证绕过 HTTP 直接调 service 时同样受控。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{
    UserAddRoleRequest, UserCreateRequest, UserListOut, UserListQuery, UserOut, UserRoleOut,
    UserUpdateRequest,
};

/// GET /api/v2/users
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<UserListQuery>,
) -> Result<Json<R<UserListOut>>, AppError> {
    let out = state.user_service.list_users(&query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users → 201
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<UserCreateRequest>,
) -> Result<(StatusCode, Json<R<UserOut>>), AppError> {
    let out = state.user_service.create_user(&req, &current).await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/users/{id}
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let out = state.user_service.get_user(id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users/{id}/update
pub async fn update_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserUpdateRequest>,
) -> Result<Json<R<UserOut>>, AppError> {
    let out = state.user_service.update_user(id, &req, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users/{id}/reset-password
pub async fn admin_reset_password(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let out = state
        .user_service
        .admin_reset_password(id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users/{id}/deactivate
pub async fn deactivate_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let out = state.user_service.deactivate_user(id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/users/{id}/roles
pub async fn list_user_roles(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<Vec<UserRoleOut>>>, AppError> {
    let out = state.user_service.list_user_roles(id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/users/{id}/roles → 201
pub async fn add_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserAddRoleRequest>,
) -> Result<(StatusCode, Json<R<UserRoleOut>>), AppError> {
    let out = state.user_service.add_role(id, &req, &current).await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// POST /api/v2/users/{id}/roles/{role_id}/remove
pub async fn remove_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path((id, role_id)): Path<(i64, i64)>,
) -> Result<Json<R<()>>, AppError> {
    state
        .user_service
        .remove_role(id, role_id, &current)
        .await?;
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
