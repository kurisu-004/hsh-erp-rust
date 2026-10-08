//! iam 域 account 端点 handler（12 个，原 user 域）
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::super::dto::{
    UserAddRoleRequest, UserCreateRequest, UserDeactivateRequest, UserListQuery,
    UserRemoveRoleRequest, UserUpdateRequest, WxBindRequest, WxUnbindRequest,
};
use super::super::vo::{UserListOut, UserOut, UserRoleOut, WxIdentityOut};

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
///
/// OCC 锚点 = `req.version`（客户端必填，缺失 → axum `Json` 提取器返 422 纯文本）。
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
///
/// OCC 锚点 = `req.version`（客户端必填，缺失 → 422 纯文本）。停用 = 软删
/// `t_user`，出参仍返被软删那一行的快照（`is_active=false` + `version+1`）。
pub async fn deactivate_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserDeactivateRequest>,
) -> Result<Json<R<UserOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .account_service
        .deactivate_user(&mut *tx, id, req.version, &current)
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
///
/// OCC 锚点 = `req.version`（被撤销那一行自己的 version，客户端必填）。
pub async fn remove_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path((id, role_id)): Path<(i64, i64)>,
    Json(req): Json<UserRemoveRoleRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .account_service
        .remove_role(&mut *tx, id, role_id, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}
/// POST /api/v2/iam/users/{id}/wx-bind —— 写端点（`state.pool.begin()`）
///
/// 把企业微信 userid 预绑定到系统账号（`t_wx_identity`）。请求体只有 `wx_user_id`；
/// `corp_id` 一律取后端配置 `state.config.wecom.corpid`（与登录侧对称）。
/// 权限守卫（`require_role(Role::Manager)`）在 service 层——与 `list_users` /
/// `add_role` 同一做法，handler 不重复校验。
/// 幂等：同一 userid 重复绑 → 200；userid 已属他人 → 40108；本账号已绑别的 userid → 40110。
pub async fn bind_wx_identity(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<WxBindRequest>,
) -> Result<Json<R<WxIdentityOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .account_service
        .bind_wx_identity(&mut *tx, id, &req, &state.config.wecom.corpid, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/iam/users/{id}/wx-bind —— 读端点（`state.pool.acquire()`，不开事务）
///
/// 未绑定 → `data: null`；已绑定 → `data` 是**单个对象**（业务上双向一对一）。
pub async fn get_wx_identity(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<Option<WxIdentityOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .account_service
        .get_wx_identity(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/wx-bind/unbind —— 写端点（`state.pool.begin()`）
///
/// 2026-10-10 硬切：旧路径 `DELETE /api/v2/iam/users/{id}/wx-bind` **已删、无 alias**
/// （本仓只用 GET + POST，不留唯一的 DELETE 路由）。返回值从 `R<Vec<WxIdentityOut>>`
/// 收敛为 `R<()>`——软删后的行快照对调用方无意义。
/// OCC 锚点 = `req.version`（绑定行自己的 version，客户端必填）。
/// 幂等：该账号当前无绑定时重复调用 → 200。
pub async fn unbind_wx_identity(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<WxUnbindRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .account_service
        .unbind_wx_identity(&mut *tx, id, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}
