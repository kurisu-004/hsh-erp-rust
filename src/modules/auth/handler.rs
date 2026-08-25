//! auth HTTP handler
//!
//! 对应 Python myERP/api/v1/auth.py。
//!
//! ## 约定
//! - 写接口（login / refresh / change-password）在 handler 开 tx 并显式 commit。
//! - 只读（me）直接 acquire 一个连接，无需事务。
//! - 公开端点（login / refresh）不注入 `CurrentUser` extractor；其余端点都需 Bearer JWT。
//! - logout 现在真删 Redis session 条目（`delete_session(token_hash)`），后续 `/me` 立即 40105。

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::extractor::AuthTokenHash;
use crate::auth::rbac::CurrentUser;
use crate::modules::user::dto::{ChangePasswordRequest, CurrentUserOut};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{LoginRequest, LoginResponse, LogoutResponse, RefreshRequest};
use super::service::AuthService;

/// POST /api/v2/auth/login
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let resp = AuthService::login(&mut tx, req, &state).await?;
    tx.commit().await?;
    Ok(Json(R::ok(resp)))
}

/// GET /api/v2/auth/me
pub async fn me(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
) -> Result<Json<R<CurrentUserOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = AuthService::me(&mut conn, &user, &state).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/auth/logout —— 删当前 token 的 Redis session，使后续 `/me` 立即 40105。
pub async fn logout(
    State(state): State<Arc<AppState>>,
    _user: CurrentUser,
    AuthTokenHash(token_hash): AuthTokenHash,
) -> Result<Json<R<LogoutResponse>>, AppError> {
    AuthService::logout(&state, &token_hash).await?;
    Ok(Json(R::ok(LogoutResponse { ok: true })))
}

/// POST /api/v2/auth/change-password
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<R<()>>, AppError> {
    let user_id = user.id;
    let mut tx = state.pool.begin().await?;
    AuthService::change_password(&mut tx, user_id, req, &user, &state).await?;
    tx.commit().await?;
    // `UserService::change_own_password` 内部已经做过 best-effort 清 session；
    // 这里无需再清——单点入口收敛到 service 层。
    Ok(Json(R::ok_empty()))
}

/// POST /api/v2/auth/refresh
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let resp = AuthService::refresh(&mut tx, req, &state).await?;
    tx.commit().await?;
    Ok(Json(R::ok(resp)))
}

/// 本域路由表（挂载点 `/api/v2/auth`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/login", post(login))
        .route("/me", get(me))
        .route("/logout", post(logout))
        .route("/change-password", post(change_password))
        .route("/refresh", post(refresh))
}
