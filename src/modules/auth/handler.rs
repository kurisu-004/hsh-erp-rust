//! auth HTTP handler
//!
//! 对应 Python myERP/api/v1/auth.py。
//!
//! ## 约定
//! - 写接口（login / refresh / change-password）在 handler 开 tx 并显式 commit。
//! - 只读（me）直接 acquire 一个连接，无需事务。
//! - 公开端点（login / refresh）不注入 `CurrentUser` extractor；其余端点都需 Bearer JWT。
//! - logout 是 no-op：服务端无法强制吊销 access token（短期有效），由 `refresh` 轮转版本号
//!   实现「实质登出」；本接口返回 `{ ok: true }` 让前端走完流程。

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};

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

/// POST /api/v2/auth/logout —— no-op；返回 `{ ok: true }`
pub async fn logout() -> Json<R<LogoutResponse>> {
    Json(R::ok(LogoutResponse { ok: true }))
}

/// POST /api/v2/auth/change-password
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    AuthService::change_password(&mut tx, user.id, req, &user).await?;
    tx.commit().await?;
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
