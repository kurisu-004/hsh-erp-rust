//! axum 中间件层：JWT + 服务端 session 校验（2026-09-20 新增）
//!
//! 历史背景：认证原本由 `CurrentUser` extractor 在 handler 参数里即时触发；100+ 处
//! `require_role` 散布在 service 层。WS dashboard 又把校验逻辑手工复制了一份。
//! 这次重构把"Token → CurrentUser / AuthTokenHash"的链路收敛到一处：
//!
//! - HTTP REST：注册 `auth_middleware` 到 `v2_router()` 顶层 `route_layer`，
//!   middleware 验证后把 `CurrentUser` / `AuthTokenHash` 写入 `req.extensions_mut()`，
//!   handler 的 extractor 退化成薄壳（仅从 extensions 读取）。
//! - WS dashboard：query-token 鉴权走 `verify_access_token` 共享核验函数（不走中间件）。
//!
//! ## 公开路径白名单
//! 不带 token 也能访问：health、login、refresh、`/_e2e/*` 全部前缀。
//!
//! ## 错误码
//! - 40100 `UNAUTHORIZED`：缺 / 坏 token、签名失败、claims 不合规
//! - 40102 `TOKEN_EXPIRED`：jwt ErrorKind::ExpiredSignature（2026-09-20 新增细分）
//! - 40105 `SESSION_REVOKED`：Redis 中查不到 / user_id 不匹配
//!
//! ## 授权（角色检查）
//! **不动** —— `CurrentUser::require_role(Role::Manager)?` 仍在 service / handler 层调用，
//! 符合 `backend-rust/CLAUDE.md` 约定 #4「权限在服务层」。
//!
//! ## 滑动 TTL
//! `touch_session` 失败仅 `tracing::warn!`，不阻断请求 —— 业务连续性 > 严格 TTL 滑动。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::extractor::AuthTokenHash;
use crate::auth::jwt::decode_access;
use crate::auth::rbac::{CurrentUser, parse_role_str_or_warn};
use crate::auth::session::hash_token;
use crate::shared::error::{AppError, code};
use crate::state::AppState;

/// 公开路径白名单（无须 token 也能访问）。
///
/// 与 `v2_router()` 的 nest 路径组合后，`req.uri().path()` 在生产是完整前缀
/// `/api/v2/...`，但集成测试的 `test_app` 直接挂 `v2_router`（不加 `/api/v2`
/// 前缀），路径就是 `/iam/login`。两种场景都要放行——本函数先尝试 strip 前缀
/// 再做精确 / 前缀匹配（不依赖 axum Nest 实际挂了哪一层）。
///
/// - 精确匹配：`/health` `/iam/login` `/iam/refresh`
/// - 前缀匹配：`/_e2e` 与 `/_e2e/`（e2e hook 整段子树都免鉴权）
fn is_public_path(path: &str) -> bool {
    let stripped = path.strip_prefix("/api/v2").unwrap_or(path);
    stripped == "/health"
        || stripped == "/iam/login"
        || stripped == "/iam/refresh"
        || stripped == "/_e2e"
        || stripped.starts_with("/_e2e/")
}

/// 校验 access token + Redis session，返回 `(CurrentUser, sha256_hex)`。
///
/// HTTP middleware 与 WS dashboard 共用本函数（避免校验逻辑两处实现漂移）。
///
/// 流程：
/// 1. `decode_access` 验签（带 iss 校验；ExpiredSignature → 40102，其余 40100）
/// 2. 若 `state.config.redis.session_check_enabled`：
///    - `state.session.get_session(&hash_token(token))`；查不到 / user_id 不匹配 → 40105
///    - `touch_session` 滑动 TTL；失败仅 warn
///    - roles 走 `parse_role_str_or_warn` 把缓存里的大写字符串转回 `Role` enum（未知值 warn+skip）
///    - shelf_ids / shelf_wildcard 从 `cached.cached` 取
/// 3. 关闭 session check 时：直接用 JWT claims 构造
pub async fn verify_access_token(
    state: &Arc<AppState>,
    token: &str,
) -> Result<(CurrentUser, String /* token_hash */), AppError> {
    let claims = decode_access(token, &state.config.jwt.secret, &state.config.jwt.issuer)?;

    let token_hash = hash_token(token);

    if state.config.redis.session_check_enabled {
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

        // 把缓存中的大写 role 字符串转回 Role enum（未知值 warn+skip）
        let mut roles = Vec::with_capacity(cached.cached.roles.len());
        for r in &cached.cached.roles {
            if let Some(role) = parse_role_str_or_warn(r) {
                roles.push(role);
            }
        }

        Ok((
            CurrentUser {
                id: claims.sub,
                username: claims.username,
                roles,
                shelf_ids: cached.cached.shelf_ids,
                shelf_wildcard: cached.cached.shelf_wildcard,
            },
            token_hash,
        ))
    } else {
        // 关闭 Redis 服务端 session 校验：直接用 JWT claims 构造 CurrentUser
        Ok((
            CurrentUser {
                id: claims.sub,
                username: claims.username,
                roles: claims.roles,
                shelf_ids: claims.shelf_ids,
                shelf_wildcard: claims.shelf_wildcard,
            },
            token_hash,
        ))
    }
}

/// axum 中间件：解析 `Authorization: Bearer <token>`，调 `verify_access_token`，
/// 把 `CurrentUser` + `AuthTokenHash` 写入 `req.extensions_mut()`，再交给下一层。
///
/// 公开路径（health / login / refresh / _e2e）直接 `next.run(req).await`，
/// 不写 extensions —— 这些端点本身不带 token，handler 也不取 `CurrentUser`。
///
/// 任何失败 → `AppError::into_response()` 直接返（不调 `next.run`），错误信封
/// 与既有 `AppError::Biz` 同形（code / message / data）。
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    // 白名单直接放行
    if is_public_path(&path) {
        return next.run(req).await;
    }

    // 取 Bearer token
    let token = match extract_bearer_token(req.headers()) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    // 校验 + 构造 CurrentUser
    let (user, token_hash) = match verify_access_token(&state, &token).await {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

    // 写入 request extensions（handler 端 `CurrentUser::from_request_parts` /
    // `AuthTokenHash::from_request_parts` 从这里读）
    req.extensions_mut().insert(user);
    req.extensions_mut().insert(AuthTokenHash(token_hash));

    next.run(req).await
}

/// 从 headers 抠 `Authorization: Bearer <token>`，找不到 → 40100。
fn extract_bearer_token(headers: &HeaderMap) -> Result<String, AppError> {
    let raw = headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "缺少 Bearer token"))?;
    raw.strip_prefix("Bearer ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "缺少 Bearer token"))
}
