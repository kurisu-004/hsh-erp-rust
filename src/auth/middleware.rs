//! axum 中间件层：JWT + 服务端 session 校验（2026-09-20 新增 + 2026-09-22 重构 + 2026-09-23 重构）
//!
//! 历史背景：认证原本由 `CurrentUser` extractor 在 handler 参数里即时触发；100+ 处
//! `require_role` 散布在 service 层。WS dashboard 又把校验逻辑手工复制了一份。
//! 这次重构把"Token → CurrentUser / SessionJti"的链路收敛到一处：
//!
//! - HTTP REST：注册 `authenticate_middleware` 到 `v2_router()` 顶层 `route_layer`，
//!   middleware 验证后把 `CurrentUser` / `SessionJti` 写入 `req.extensions_mut()`，
//!   handler 的 extractor 退化成薄壳（仅从 extensions 读取）。
//! - WS dashboard：query-token 鉴权走 `verify_session_token` 共享核验函数（不走中间件）。
//!
//! ## 公开路径白名单
//! 不带 token 也能访问：health、login、refresh、`/_e2e/*` 全部前缀。
//!
//! ## 错误码
//! - 40100 `UNAUTHORIZED`：缺 / 坏 token、签名失败、claims 不合规（含缺 `aud` / `sub`
//!   等必填 claim；详见 `jwt::decode_access` 的 `set_required_spec_claims`）
//! - 40102 `TOKEN_EXPIRED`：jwt ErrorKind::ExpiredSignature
//! - 40105 `SESSION_REVOKED`：Redis 中查不到 / user_id 不匹配（session 真源已吊销）
//! - 50000 `INTERNAL`：`session_check_enabled=false` 配置错误；强制 prod 必须开启 Redis，
//!   关闭时直接 5xxxx 让运维感知（而非 40105 让用户被踢下线困惑）
//!
//! ## 授权（角色检查）
//! **不动** —— `CurrentUser::require_role(Role::Manager)?` 仍在 service / handler 层调用，
//! 符合 `backend-rust/CLAUDE.md` 约定 #4「权限在服务层」。
//!
//! ## 滑动 TTL
//! `touch_session` 失败仅 `tracing::warn!`，不阻断请求 —— 业务连续性 > 严格 TTL 滑动。
//!
//! ## 2026-09-22 重构要点
//! - `auth_middleware` → `authenticate_middleware` / `verify_access_token` → `verify_session_token`
//!   / `extract_bearer_token` → `extract_bearer_authorization`：全词化命名。
//! - session_check=false 路径**直接返回** `INTERNAL` (50000)：access token 已不再携带业务
//!   字段，关闭 session check 后服务端无法从 JWT 重建 `CurrentUser`，强制 prod 必须开启 Redis；
//!   5xxxx 让运维感知配置错误，而非 40105 SESSION_REVOKED 让用户被踢下线困惑。
//! - `Claims` → `AccessTokenClaims`：JWT 字段全词化（subject/audience/issued_at/...），
//!   通过 `#[serde(rename = "...")]` 桥接 RFC 7519 短码。
//!
//! ## 2026-09-23 重构要点
//! - session key 由 `sha256(token)` 改为 JWT 自带 jti（UUID v4）：
//!   Redis 主条目 key 现在是 `session:tok:<jti>`，`SessionStore` 入参从 `token_hash`
//!   改为 `jti`，`hash_token` 函数被删除（`sha2` crate 同步移除）。
//! - `AuthenticatedTokenHash` 重命名为 `SessionJti`，值类型仍为 `String`，但语义
//!   从 sha256 hex 改为 jti UUID v4。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::extractor::SessionJti;
use crate::auth::jwt::decode_access;
use crate::auth::rbac::{CurrentUser, parse_role_string};
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

/// 校验 access token + Redis session，返回 `(CurrentUser, jti UUID v4)`。
///
/// HTTP middleware 与 WS dashboard 共用本函数（避免校验逻辑两处实现漂移）。
///
/// 流程：
/// 1. `decode_access` 验签（带 iss/aud 校验 + `set_required_spec_claims`；
///    ExpiredSignature → 40102，其余 40100）
/// 2. 必须 `state.config.redis.session_check_enabled=true`：从 `state.session.get_session`
///    查 Redis；查不到 / user_id 不匹配 → 40105；通过则继续。
///    **关闭 session check 时直接返回 50000 INTERNAL**（2026-09-22 重构：原 40105）：
///    access token 不再携带业务字段，服务端无法从 JWT 重建 `CurrentUser`，
///    强制 prod 必须开启 Redis；5xxxx 让运维感知配置错误。
/// 3. `touch_session` 滑动 TTL；失败仅 warn
/// 4. roles 走 `parse_role_string` 把缓存里的大写字符串转回 `Role` enum（未知值 warn+skip）
/// 5. username/roles/shelf_ids/shelf_wildcard 从 `cached.profile.{...}` 取（注意字段名是
///    `profile`，不是 `cached`；2026-09-22 重命名）
///
/// ## 2026-09-23 重构：Redis session key 直接使用 JWT 自带的 jti
///
/// 注意：Redis session 的 key 是 JWT payload 里的 `claims.jwt_id`（UUID v4），
/// **无法**通过对 token 做 sha256 派生匹配——sha256 路径在本轮已被移除。
pub async fn verify_session_token(
    state: &Arc<AppState>,
    token: &str,
) -> Result<(CurrentUser, String /* jti */), AppError> {
    // 1) JWT 验签（带 iss + aud 校验）
    let claims = decode_access(
        token,
        &state.config.jwt.secret,
        &state.config.jwt.issuer,
        &state.config.jwt.audience,
    )?;

    // 2026-09-23 重构：session key 直接用 JWT 自带 jti（claims.jwt_id），
    // 不再对 token 做 sha256 哈希。
    let jti = claims.jwt_id.clone();

    // 2) session check gate：关掉则直接拒（强制 prod 必须开 Redis）
    //
    // 2026-09-22 重构：此分支映射到 `INTERNAL` (50000) 而非 `SESSION_REVOKED` (40105)。
    // `SESSION_REVOKED` 语义是"用户会话已被吊销"，前端会清 token 跳登录页；
    // 但 `session_check_enabled=false` 实际是**配置错误/部署阶段**问题——
    // 应让运维立刻看到 5xxxx 错误触发响应，而不是让用户被踢下线困惑。
    if !state.config.redis.session_check_enabled {
        return Err(AppError::internal(
            "Redis session check 必须开启（access token 已不再携带业务字段）",
        ));
    }

    // 3) 查 Redis session
    let cached = state
        .session
        .get_session(&jti)
        .await?
        .ok_or_else(|| AppError::biz(code::SESSION_REVOKED, "会话已被吊销，请重新登录"))?;
    if cached.user_id != claims.subject {
        return Err(AppError::biz(
            code::SESSION_REVOKED,
            "会话已被吊销，请重新登录",
        ));
    }

    // 4) 滑动 TTL（best-effort；失败仅 warn，不阻断请求）
    if let Err(e) = state
        .session
        .touch_session(&jti, state.config.redis.session_ttl_seconds)
        .await
    {
        tracing::warn!(error = %e, "刷新 session TTL 失败");
    }

    // 5) 把缓存中的大写 role 字符串转回 Role enum（未知值 warn+skip）
    let mut roles = Vec::with_capacity(cached.profile.roles.len());
    for r in &cached.profile.roles {
        if let Some(role) = parse_role_string(r) {
            roles.push(role);
        }
    }

    Ok((
        CurrentUser {
            id: cached.user_id,
            username: cached.profile.username,
            roles,
            shelf_ids: cached.profile.shelf_ids,
            shelf_wildcard: cached.profile.shelf_wildcard,
        },
        jti,
    ))
}

/// axum 中间件：解析 `Authorization: Bearer <token>`，调 `verify_session_token`，
/// 把 `CurrentUser` + `SessionJti` 写入 `req.extensions_mut()`，再交给下一层。
///
/// 公开路径（health / login / refresh / _e2e）直接 `next.run(req).await`，
/// 不写 extensions —— 这些端点本身不带 token，handler 也不取 `CurrentUser`。
///
/// 任何失败 → `AppError::into_response()` 直接返（不调 `next.run`），错误信封
/// 与既有 `AppError::Biz` 同形（code / message / data）。
pub async fn authenticate_middleware(
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
    let token = match extract_bearer_authorization(req.headers()) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    // 校验 + 构造 CurrentUser
    let (user, jti) = match verify_session_token(&state, &token).await {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

    // 写入 request extensions（handler 端 `CurrentUser::from_request_parts` /
    // `SessionJti::from_request_parts` 从这里读）
    req.extensions_mut().insert(user);
    req.extensions_mut().insert(SessionJti(jti));

    next.run(req).await
}

/// 从 headers 抠 `Authorization: Bearer <token>`，找不到 → 40100。
fn extract_bearer_authorization(headers: &HeaderMap) -> Result<String, AppError> {
    let raw = headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "缺少 Bearer token"))?;
    raw.strip_prefix("Bearer ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "缺少 Bearer token"))
}
