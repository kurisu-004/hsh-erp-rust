//! iam 域 session 端点 handler（5 个，原 auth 域）
//!
//! ## 事务边界（2026-09-21 重构 + 2026-09-22 删 `PgIamRepo` 转发壳）
//! - `login` / `refresh`：两阶段——handler `pool.begin()` → service → commit →
//!   service.complete_login/complete_refresh（commit 后写 Redis）。
//! - `change_password`：handler `pool.begin()` → service → commit → handler 清 Redis
//!   session（best-effort；DB 的 refresh_token_version 轮转是兜底）。
//! - `me`：读端点，`pool.acquire()` 不开事务。
//! - `logout`：无 DB 操作，仅删 Redis session。
//!
//! 全部端点要求 Bearer JWT（除 `login` / `refresh` 是公开），权限守卫在 service 层。

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::auth::extractor::AuthenticatedTokenHash;
use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::super::dto::{ChangePasswordRequest, LoginRequest, RefreshRequest};
use super::super::vo::{CurrentUserOut, LoginResponse, LogoutResponse};

/// POST /api/v2/iam/login —— 写端点（login 两阶段：DB 在 tx 内，Redis session 在 commit 后）
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let pending = state.session_service.login(&mut *tx, req).await?;
    tx.commit().await?;
    let resp = state.session_service.complete_login(pending).await?;
    Ok(Json(R::ok(resp)))
}

/// GET /api/v2/iam/me —— 读端点，acquire 不开事务
pub async fn me(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
) -> Result<Json<R<CurrentUserOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state.session_service.me(&mut *conn, &user).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/logout —— 删当前 token 的 Redis session，使后续 `/me` 立即 40105。
/// 无 DB 操作。
pub async fn logout(
    State(state): State<Arc<AppState>>,
    _user: CurrentUser,
    AuthenticatedTokenHash(token_hash): AuthenticatedTokenHash,
) -> Result<Json<R<LogoutResponse>>, AppError> {
    state.session_service.logout(&token_hash).await?;
    Ok(Json(R::ok(LogoutResponse { ok: true })))
}

/// POST /api/v2/iam/change-password —— 写端点 + post-commit 清 Redis session
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<R<()>>, AppError> {
    let user_id = user.id;
    let mut tx = state.pool.begin().await?;
    state
        .session_service
        .change_password(&mut *tx, user_id, req, &user)
        .await?;
    tx.commit().await?;
    // commit 之后清该用户的 Redis session（best-effort；DB 的 refresh_token_version 轮转是兜底）
    if let Err(e) = state.session.delete_all_user_sessions(user_id).await {
        tracing::warn!(error = %e, user_id, "change_password: 清 session 失败");
    }
    Ok(Json(R::ok_empty()))
}

/// POST /api/v2/iam/refresh —— 写端点（refresh 两阶段：DB 在 tx 内，旧/新 Redis session 在 commit 后）
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let pending = state.session_service.refresh(&mut *tx, req).await?;
    tx.commit().await?;
    let resp = state.session_service.complete_refresh(pending).await?;
    Ok(Json(R::ok(resp)))
}