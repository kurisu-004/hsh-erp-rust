//! iam 域 account 端点 handler（9 个，原 user 域）
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::super::dto::{
    UserAddRoleRequest, UserCreateRequest, UserListQuery, UserUpdateRequest,
};
use super::super::vo::{UserListOut, UserOut, UserRoleOut};

/// GET /api/v2/iam/users —— 读端点，acquire 不开事务
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<UserListQuery>,
) -> Result<Json<R<UserListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .account_service
        .list_users(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users → 201 —— 纯写端点
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<UserCreateRequest>,
) -> Result<(StatusCode, Json<R<UserOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .account_service
        .create_user(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/iam/users/{id} —— 读端点，acquire 不开事务
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .account_service
        .get_user(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/update —— 纯写端点
pub async fn update_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserUpdateRequest>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .account_service
        .update_user(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/reset-password —— 写端点 + post-commit 清 session
pub async fn admin_reset_password(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .account_service
        .admin_reset_password(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    // commit 之后清该用户的 Redis session（best-effort）
    if let Err(e) = state.session.delete_all_user_sessions(id).await {
        tracing::warn!(error = %e, user_id = id, "admin_reset_password: 清 session 失败");
    }
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/deactivate —— 纯写端点
pub async fn deactivate_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .account_service
        .deactivate_user(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/iam/users/{id}/roles —— 读端点，acquire 不开事务
pub async fn list_user_roles(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<Vec<UserRoleOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .account_service
        .list_user_roles(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/roles → 201 —— 纯写端点
pub async fn add_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserAddRoleRequest>,
) -> Result<(StatusCode, Json<R<UserRoleOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .account_service
        .add_role(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// POST /api/v2/iam/users/{id}/roles/{role_id}/remove —— 纯写端点
pub async fn remove_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path((id, role_id)): Path<(i64, i64)>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .account_service
        .remove_role(&mut *tx, id, role_id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}