//! iam 域 HTTP handler 入口（router 工厂）

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

use crate::state::AppState;

mod account;
mod session;

/// iam 域 router（挂在 `/api/v2/iam`，见 `modules::v2_router`）
///
/// 端点拆分：
/// - session 端点（`session.rs`，5 个）：挂在 `/iam` 根
/// - account 端点（`account.rs`，9 个）：挂在 `/iam/users`
///
/// 2026-09-19 IAM 域收尾（PR-4）：`auth_router` / `users_router` 旧 alias 已下线；
/// 新路径 `/api/v2/iam/*` 是 IAM 域唯一对外接口。
pub fn router() -> Router<Arc<AppState>> {
    let session = Router::new()
        .route("/login", post(session::login))
        .route("/me", get(session::me))
        .route("/logout", post(session::logout))
        .route("/change-password", post(session::change_password))
        .route("/refresh", post(session::refresh));
    let users = Router::new()
        .route("/", get(account::list_users).post(account::create_user))
        .route("/{id}", get(account::get_user))
        .route("/{id}/update", post(account::update_user))
        .route("/{id}/reset-password", post(account::admin_reset_password))
        .route("/{id}/deactivate", post(account::deactivate_user))
        .route("/{id}/roles", get(account::list_user_roles).post(account::add_role))
        .route(
            "/{id}/roles/{role_id}/remove",
            post(account::remove_role),
        );
    Router::new()
        .merge(session)
        .nest("/users", users)
}