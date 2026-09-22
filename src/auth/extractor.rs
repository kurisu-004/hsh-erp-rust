//! axum extractor：从 request extensions 读取 `CurrentUser` / `AuthenticatedTokenHash`
//!
//! 2026-09-20 重构：原本的 JWT 验签 + Redis session 校验 + 滑动 TTL 全部迁移到
//! `auth::middleware::authenticate_middleware`；本模块**仅**作为薄壳，从
//! `req.extensions()` 读 `CurrentUser` / `AuthenticatedTokenHash`，由 middleware 注入。
//!
//! Handler 用法（不变）：
//! ```ignore
//! async fn handler(
//!     user: CurrentUser,
//!     State(state): State<Arc<AppState>>,
//!     Json(req): Json<MyReq>,
//! ) -> Result<Json<R<MyOut>>, AppError> {
//!     user.require_role(Role::Manager)?;
//!     ...
//! }
//! ```
//!
//! ## fail-closed 设计
//! 取不到 `CurrentUser` / `AuthenticatedTokenHash`（例如 middleware 未挂、或 whitelist
//! 路径下 handler 误取）→ `AppError::biz(code::UNAUTHORIZED, ...)`（40100）。
//! 报错信息含「middleware misconfigured」提示，便于主代理排查配置问题。

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::auth::rbac::CurrentUser;
use crate::shared::error::{AppError, code};

impl FromRequestParts<Arc<crate::state::AppState>> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &Arc<crate::state::AppState>,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .ok_or_else(|| {
                AppError::biz(
                    code::UNAUTHORIZED,
                    "missing CurrentUser in request extensions (middleware misconfigured)",
                )
            })
    }
}

/// Token hash（由 middleware 写入 extensions，供 logout 等 handler 拿 sha256 去删 Redis）
///
/// 2026-09-22 重命名：原 `AuthTokenHash` → `AuthenticatedTokenHash`，与 `authenticate_middleware`
/// 命名风格对齐（已鉴权产物）。
#[derive(Clone)]
pub struct AuthenticatedTokenHash(pub String);

impl FromRequestParts<Arc<crate::state::AppState>> for AuthenticatedTokenHash {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &Arc<crate::state::AppState>,
    ) -> Result<Self, Self::Rejection> {
        // 同样取不到 → 40100 fail-closed
        parts
            .extensions
            .get::<AuthenticatedTokenHash>()
            .cloned()
            .ok_or_else(|| {
                AppError::biz(
                    code::UNAUTHORIZED,
                    "missing AuthenticatedTokenHash in request extensions (middleware misconfigured)",
                )
            })
    }
}
