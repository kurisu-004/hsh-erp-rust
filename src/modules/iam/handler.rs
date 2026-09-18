//! iam 域 HTTP handler（auth + account 二合一）
//!
//! 对应 Python myERP/api/v1/auth.py + api/v1/user.py。
//!
//! ## 约定（2026-09-18 auth-di 重构 Wave 2B + 2026-09-19 IAM 合并）
//! - 14 端点全部薄壳化：handler 只做 extractor 取参 + `state.session_service.xxx(...)` 或
//!   `state.account_service.xxx(...)` 调用 + `R::ok(...)` 信封包装。**无 begin / 无 commit /
//!   无 acquire**——事务边界在 service。
//! - 公开端点（login / refresh）不注入 `CurrentUser` extractor；其余端点都需 Bearer JWT。
//! - logout 通过 `AuthTokenHash` extractor 拿当前 token 的 sha256 hex，调
//!   `state.session_service.logout(...)` 删 Redis session 条目，后续 `/iam/me` 立即 40105。
//!
//! ## 路由双注册（PR-1 兼容期）
//! `auth_router()` / `users_router()` 是 `/api/v2/auth/*` / `/api/v2/users/*` 的旧 alias，
//! handler 与 `router()` 共享（同一组 axum 路由函数挂到不同 nest 上）。保留期给前端 /
//! 第三方客户端做迁移缓冲；PR-4（计划）会删。
//!
//! 2026-09-19 IAM 域合并：合并 `auth/handler.rs`（5 端点）+ `user/handler.rs`（9 端点），
//! 业务 handler 函数 + router 工厂函数一并落到本文件。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::extractor::AuthTokenHash;
use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{
    ChangePasswordRequest, CurrentUserOut, LoginRequest, LoginResponse, LogoutResponse,
    RefreshRequest, UserAddRoleRequest, UserCreateRequest, UserListOut, UserListQuery, UserOut,
    UserRoleOut, UserUpdateRequest,
};

// ===========================================================================
// Session 端点（5 个，原 auth 域）
// ===========================================================================

/// POST /api/v2/iam/login
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let resp = state.session_service.login(req).await?;
    Ok(Json(R::ok(resp)))
}

/// GET /api/v2/iam/me
pub async fn me(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
) -> Result<Json<R<CurrentUserOut>>, AppError> {
    let out = state.session_service.me(&user).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/logout —— 删当前 token 的 Redis session，使后续 `/me` 立即 40105。
pub async fn logout(
    State(state): State<Arc<AppState>>,
    _user: CurrentUser,
    AuthTokenHash(token_hash): AuthTokenHash,
) -> Result<Json<R<LogoutResponse>>, AppError> {
    state.session_service.logout(&token_hash).await?;
    Ok(Json(R::ok(LogoutResponse { ok: true })))
}

/// POST /api/v2/iam/change-password
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<R<()>>, AppError> {
    let user_id = user.id;
    state
        .session_service
        .change_password(user_id, req, &user)
        .await?;
    // `AccountService::change_own_password` 内部已经做过 best-effort 清 session；
    // 这里无需再清——单点入口收敛到 service 层。
    Ok(Json(R::ok_empty()))
}

/// POST /api/v2/iam/refresh
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let resp = state.session_service.refresh(req).await?;
    Ok(Json(R::ok(resp)))
}

// ===========================================================================
// Account 端点（9 个，原 user 域）
// ===========================================================================

/// GET /api/v2/iam/users
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<UserListQuery>,
) -> Result<Json<R<UserListOut>>, AppError> {
    let out = state.account_service.list_users(&query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users → 201
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<UserCreateRequest>,
) -> Result<(StatusCode, Json<R<UserOut>>), AppError> {
    let out = state.account_service.create_user(&req, &current).await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/iam/users/{id}
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let out = state.account_service.get_user(id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/update
pub async fn update_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserUpdateRequest>,
) -> Result<Json<R<UserOut>>, AppError> {
    let out = state
        .account_service
        .update_user(id, &req, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/reset-password
pub async fn admin_reset_password(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let out = state
        .account_service
        .admin_reset_password(id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/deactivate
pub async fn deactivate_user(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<UserOut>>, AppError> {
    let out = state.account_service.deactivate_user(id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/iam/users/{id}/roles
pub async fn list_user_roles(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<Vec<UserRoleOut>>>, AppError> {
    let out = state.account_service.list_user_roles(id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/users/{id}/roles → 201
pub async fn add_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UserAddRoleRequest>,
) -> Result<(StatusCode, Json<R<UserRoleOut>>), AppError> {
    let out = state.account_service.add_role(id, &req, &current).await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// POST /api/v2/iam/users/{id}/roles/{role_id}/remove
pub async fn remove_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path((id, role_id)): Path<(i64, i64)>,
) -> Result<Json<R<()>>, AppError> {
    state
        .account_service
        .remove_role(id, role_id, &current)
        .await?;
    Ok(Json(R::ok_empty()))
}

// ===========================================================================
// 路由工厂函数（3 个：router / auth_router / users_router）
//
// `auth_router` 与 `users_router` 是 PR-1 兼容期的旧 alias；同组路由函数挂到不同
// nest 上。前端在 PR-4 之前可继续用 `/api/v2/auth/*` + `/api/v2/users/*`。
// ===========================================================================

/// 新路径 router（挂在 `/api/v2/iam`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    let session = Router::new()
        .route("/login", post(login))
        .route("/me", get(me))
        .route("/logout", post(logout))
        .route("/change-password", post(change_password))
        .route("/refresh", post(refresh));
    let users = Router::new()
        .route("/", get(list_users).post(create_user))
        .route("/{id}", get(get_user))
        .route("/{id}/update", post(update_user))
        .route("/{id}/reset-password", post(admin_reset_password))
        .route("/{id}/deactivate", post(deactivate_user))
        .route("/{id}/roles", get(list_user_roles).post(add_role))
        .route("/{id}/roles/{role_id}/remove", post(remove_role));
    Router::new()
        // session 端点挂在 `/iam` 根
        .merge(session)
        // account 端点挂在 `/iam/users`
        .nest("/users", users)
}

/// 旧 alias router（挂在 `/api/v2/auth`，PR-1 兼容期保留，PR-4 删除）
pub fn auth_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/login", post(login))
        .route("/me", get(me))
        .route("/logout", post(logout))
        .route("/change-password", post(change_password))
        .route("/refresh", post(refresh))
}

/// 旧 alias router（挂在 `/api/v2/users`，PR-1 兼容期保留，PR-4 删除）
pub fn users_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_users).post(create_user))
        .route("/{id}", get(get_user))
        .route("/{id}/update", post(update_user))
        .route("/{id}/reset-password", post(admin_reset_password))
        .route("/{id}/deactivate", post(deactivate_user))
        .route("/{id}/roles", get(list_user_roles).post(add_role))
        .route("/{id}/roles/{role_id}/remove", post(remove_role))
}
