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
//!
//! ## 服务端 session 校验（可关闭）
//! 每次解析都查 Redis `session:tok:<sha256(token)>`：
//! - 不存在 → `SESSION_REVOKED`（40105），强制重新登录
//! - 存在但 `user_id` 与 JWT claims 不一致 → `SESSION_REVOKED`
//! - 通过后滑动 TTL（`EXPIRE`）；失败仅 warn，不阻断请求
//!
//! 当 `REDIS_SESSION_CHECK_ENABLED=false`（Rust 借 Python JWT 的迁移过渡期），
//! 跳过 Redis 查询，直接从 claims 构造 `CurrentUser`。

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use crate::auth::jwt::decode_access;
use crate::auth::rbac::{parse_role_str_or_warn, CurrentUser};
use crate::auth::session::hash_token;
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

        if state.config.redis.session_check_enabled {
            // 服务端 session 校验：Redis 中必须存在 sha256(token) 对应的条目。
            let token_hash = hash_token(token);
            let cached = state
                .session
                .get_session(&token_hash)
                .await?
                .ok_or_else(|| AppError::biz(code::SESSION_REVOKED, "会话已被吊销，请重新登录"))?;
            if cached.user_id != claims.sub {
                return Err(AppError::biz(
                    code::SESSION_REVOKED,
                    "会话已被吊销，请重新登录",
                ));
            }

            // 滑动 TTL（best-effort；失败仅 warn，不阻断请求）
            if let Err(e) = state
                .session
                .touch_session(&token_hash, state.config.redis.session_ttl_seconds)
                .await
            {
                tracing::warn!(error = %e, "刷新 session TTL 失败");
            }

            // 把缓存中的大写 role 字符串转回 Role enum（未知值走 `parse_role_str_or_warn` 跳过）
            let mut roles = Vec::with_capacity(cached.cached.roles.len());
            for r in &cached.cached.roles {
                if let Some(role) = parse_role_str_or_warn(r) {
                    roles.push(role);
                }
            }

            Ok(CurrentUser {
                id: claims.sub,
                username: claims.username,
                roles,
                shelf_ids: cached.cached.shelf_ids,
                shelf_wildcard: cached.cached.shelf_wildcard,
            })
        } else {
            // 关闭 Redis 服务端 session 校验：直接用 JWT claims 构造 CurrentUser
            Ok(CurrentUser {
                id: claims.sub,
                username: claims.username,
                roles: claims.roles,
                shelf_ids: claims.shelf_ids,
                shelf_wildcard: claims.shelf_wildcard,
            })
        }
    }
}

/// 第二个 extractor：仅取 Bearer token 并返回 sha256 hex，**不查 Redis**。
///
/// 用途：handler 想拿到 token 哈希去做进一步动作（如 logout 调用
/// `SessionStore::delete_session(token_hash)`）。
pub struct AuthTokenHash(pub String);

impl FromRequestParts<Arc<AppState>> for AuthTokenHash {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "缺少 Bearer token"))?;
        Ok(Self(hash_token(token)))
    }
}