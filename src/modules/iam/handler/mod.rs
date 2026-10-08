//! iam 域 HTTP handler 入口（router 工厂）

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

use crate::modules::iam::shelf;
use crate::state::AppState;

mod account;
mod session;

/// iam 域 router（挂在 `/api/v2/iam`，见 `modules::v2_router`）
///
/// 端点拆分（account 12 + session 5 + shelf 5 = 22）：
/// - session 端点（`session.rs`，5 个）：挂在 `/iam` 根
/// - account 端点（`account.rs`，12 个）：挂在 `/iam/users`
///   （9 个账号 CRUD/角色/改密 + 3 个企业微信绑定）
/// - shelf 端点（`iam::shelf::handler`，5 个）：挂在 `/iam/shelves`，
///   由嵌套子模块自带 router 工厂，本文件只做挂载（详见 `iam/shelf/mod.rs`）
///
/// ## 硬切记录（2026-10-10）
/// - 5 条货架端点自 `/api/v2/shelves/*` 迁到 `/api/v2/iam/shelves/*`，
///   **硬切无 alias**，旧路径 404。
/// - `DELETE /api/v2/iam/users/{id}/wx-bind` **已删、无 alias** → 改为
///   `POST /api/v2/iam/users/{id}/wx-bind/unbind`（本仓只用 GET + POST；
///   不留全后端唯一的 DELETE 路由）。两者段数不同（3 vs 4），matchit 无需考虑
///   「静态段先于 catch-all」的注册顺序约束。
/// - `GET /api/v2/iam/users/{id}/wx-bind` 返回值由 `Vec<WxIdentityOut>` 改为
///   `Option<WxIdentityOut>`（业务上双向一对一）。
/// - `POST /api/v2/iam/users/{id}/wx-bind` 请求体删掉 `corp_id`（它此前是
///   「保留字段、一律忽略」）。
/// - 4 个写端点 body 新增**必填** `version`：`/{id}/update`、`/{id}/deactivate`、
///   `/{id}/roles/{role_id}/remove`、`/{id}/wx-bind/unbind`。缺失 → HTTP 422 纯文本
///   （axum `Json` 提取器，不是业务信封）。
///   （`/{id}/reset-password` 是 OCC 豁免的幂等端点、`/{id}/roles` 是纯 INSERT，
///   两者都不收 version。）
/// - 2026-09-19 IAM 域收尾（PR-4）：`auth_router` / `users_router` 旧 alias 已下线；
///   新路径 `/api/v2/iam/*` 是 IAM 域唯一对外接口。
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
        .route(
            "/{id}/roles",
            get(account::list_user_roles).post(account::add_role),
        )
        .route("/{id}/roles/{role_id}/remove", post(account::remove_role))
        // 企业微信身份预绑定：GET/POST 同一路径（查 / 绑），解绑走子路径
        .route(
            "/{id}/wx-bind",
            get(account::get_wx_identity).post(account::bind_wx_identity),
        )
        .route("/{id}/wx-bind/unbind", post(account::unbind_wx_identity));
    // 2026-10-10：货架子模块迁入 iam，路由自 `/api/v2/shelves/*` 硬切到
    // `/api/v2/iam/shelves/*`（无 alias，旧路径 404）。
    Router::new()
        .merge(session)
        .nest("/users", users)
        .nest("/shelves", shelf::router())
}
