//! axum extractor：从 Authorization: Bearer 解析为 CurrentUser
//!
//! Handler 用法：
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
//! Router 的 state 类型必须是 `Arc<AppState>`（main.rs 已设置）。

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use crate::auth::jwt::decode_access;
use crate::auth::rbac::CurrentUser;
use crate::shared::error::{code, AppError};
use crate::state::AppState;

impl FromRequestParts<Arc<AppState>> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "缺少 Bearer token"))?;

        let claims = decode_access(token, &state.config.jwt.secret, &state.config.jwt.issuer)?;

        Ok(CurrentUser {
            id: claims.sub,
            username: claims.username,
            roles: claims.roles,
            shelf_ids: claims.shelf_ids,
            shelf_wildcard: claims.shelf_wildcard,
        })
    }
}