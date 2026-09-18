//! auth HTTP handler
//!
//! 对应 Python myERP/api/v1/auth.py。
//!
//! ## 约定（2026-09-18 auth-di 重构 Wave 2B）
//! - 5 端点全部薄壳化：handler 只做 extractor 取参 + `state.auth_service.xxx(...)` 调用 +
//!   `R::ok(...)` 信封包装。**无 begin / 无 commit / 无 acquire**——事务边界在 service。
//! - 公开端点（login / refresh）不注入 `CurrentUser` extractor；其余端点都需 Bearer JWT。
//! - logout 通过 `AuthTokenHash` extractor 拿当前 token 的 sha256 hex，调 `state.auth_service.logout(...)`
//!   删 Redis session 条目，后续 `/me` 立即 40105。

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

/// POST /api/v2/auth/login
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let resp = state.auth_service.login(req).await?;
    Ok(Json(R::ok(resp)))
}

/// GET /api/v2/auth/me
pub async fn me(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
) -> Result<Json<R<CurrentUserOut>>, AppError> {
    let out = state.auth_service.me(&user).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/auth/logout —— 删当前 token 的 Redis session，使后续 `/me` 立即 40105。
pub async fn logout(
    State(state): State<Arc<AppState>>,
    _user: CurrentUser,
    AuthTokenHash(token_hash): AuthTokenHash,
) -> Result<Json<R<LogoutResponse>>, AppError> {
    state.auth_service.logout(&token_hash).await?;
    Ok(Json(R::ok(LogoutResponse { ok: true })))
}

/// POST /api/v2/auth/change-password
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<R<()>>, AppError> {
    let user_id = user.id;
    state
        .auth_service
        .change_password(user_id, req, &user)
        .await?;
    // `UserService::change_own_password` 内部已经做过 best-effort 清 session；
    // 这里无需再清——单点入口收敛到 service 层。
    Ok(Json(R::ok_empty()))
}

/// POST /api/v2/auth/refresh
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let resp = state.auth_service.refresh(req).await?;
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
