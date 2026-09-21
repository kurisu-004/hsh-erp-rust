//! iam 域 HTTP handler（auth + account 二合一）
//!
//! 对应 Python myERP/api/v1/auth.py + api/v1/user.py。
//!
//! ## 事务边界（2026-09-21 事务分层重构）
//! handler 负责 `pool.begin()` / `tx.commit()`——与 20 个 handler 文件现状对齐：
//! - ① **纯写端点**（create_user / update_user / deactivate_user / add_role / remove_role）：
//!   `pool.begin()` → service call → `tx.commit()`，错误路径 tx drop 隐式回滚。
//! - ② **写 + post-commit 副作用**（login / refresh / change_password / admin_reset_password）：
//!   `pool.begin()` → service 跑 DB → `tx.commit()` → handler 写 Redis session 或删 session
//!   （best-effort，DB 的 refresh_token_version 轮转是兜底）。复用 plan v4 §3 V6 约定。
//! - ③ **读端点**（list_users / get_user / list_user_roles / me）：`pool.acquire()` 不开
//!   事务，service 借连接执行查询，用完即 drop。
//!
//! service 仅业务逻辑（方法签名 `<R: IamRepo>(&self, repo: &mut R, ...)`），不知事务。
//!
//! ## 公开端点
//! `login` / `refresh` 不注入 `CurrentUser`；其余端点都需 Bearer JWT。logout 通过
//! `AuthTokenHash` extractor 拿当前 token 的 sha256 hex。
//!
//! 2026-09-19 IAM 域合并：合并 `auth/handler.rs`（5 端点）+ `user/handler.rs`（9 端点）。
//! 2026-09-19 IAM 域收尾（PR-4）：旧 alias `/api/v2/auth/*` + `/api/v2/users/*` 已下线。
//! 2026-09-21 事务分层重构：handler 接管 `begin` / `commit`，service 改为参数化 repo。

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
use super::repo::PgIamRepo;

// ===========================================================================
// Session 端点（5 个，原 auth 域）
// ===========================================================================

/// POST /api/v2/iam/login —— 写端点（login 两阶段：DB 在 tx 内，Redis session 在 commit 后）
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let pending = {
        let mut repo = PgIamRepo::new(&mut tx);
        state.session_service.login(&mut repo, req).await?
    };
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
    let mut repo = PgIamRepo::new(&mut conn);
    let out = state.session_service.me(&mut repo, &user).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/iam/logout —— 删当前 token 的 Redis session，使后续 `/me` 立即 40105。
/// 无 DB 操作。
pub async fn logout(
    State(state): State<Arc<AppState>>,
    _user: CurrentUser,
    AuthTokenHash(token_hash): AuthTokenHash,
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
    {
        let mut tx = state.pool.begin().await?;
        {
            let mut repo = PgIamRepo::new(&mut tx);
            state
                .session_service
                .change_password(&mut repo, user_id, req, &user)
                .await?;
        }
        tx.commit().await?;
    }
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
    let pending = {
        let mut repo = PgIamRepo::new(&mut tx);
        state.session_service.refresh(&mut repo, req).await?
    };
    tx.commit().await?;
    let resp = state.session_service.complete_refresh(pending).await?;
    Ok(Json(R::ok(resp)))
}

// ===========================================================================
// Account 端点（9 个，原 user 域）
// ===========================================================================

/// GET /api/v2/iam/users —— 读端点，acquire 不开事务
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<UserListQuery>,
) -> Result<Json<R<UserListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let mut repo = PgIamRepo::new(&mut conn);
    let out = state
        .account_service
        .list_users(&mut repo, &query, &current)
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
    let out = {
        let mut repo = PgIamRepo::new(&mut tx);
        state
            .account_service
            .create_user(&mut repo, &req, &current)
            .await?
    };
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
    let mut repo = PgIamRepo::new(&mut conn);
    let out = state
        .account_service
        .get_user(&mut repo, id, &current)
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
    let out = {
        let mut repo = PgIamRepo::new(&mut tx);
        state
            .account_service
            .update_user(&mut repo, id, &req, &current)
            .await?
    };
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
    let out = {
        let mut repo = PgIamRepo::new(&mut tx);
        state
            .account_service
            .admin_reset_password(&mut repo, id, &current)
            .await?
    };
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
    let out = {
        let mut repo = PgIamRepo::new(&mut tx);
        state
            .account_service
            .deactivate_user(&mut repo, id, &current)
            .await?
    };
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
    let mut repo = PgIamRepo::new(&mut conn);
    let out = state
        .account_service
        .list_user_roles(&mut repo, id, &current)
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
    let out = {
        let mut repo = PgIamRepo::new(&mut tx);
        state
            .account_service
            .add_role(&mut repo, id, &req, &current)
            .await?
    };
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
    {
        let mut repo = PgIamRepo::new(&mut tx);
        state
            .account_service
            .remove_role(&mut repo, id, role_id, &current)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

// ===========================================================================
// 路由工厂函数（1 个：router）
//
// 2026-09-19 IAM 域收尾（PR-4）：`auth_router` / `users_router` 旧 alias 已下线；
// 新路径 `/api/v2/iam/*` 是 IAM 域唯一对外接口。
// ===========================================================================

/// iam 域 router（挂在 `/api/v2/iam`，见 `modules::v2_router`）
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
