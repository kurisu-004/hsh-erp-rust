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
//! - `Claims` → `AccessTokenClaims`：JWT 字段全词化（subject/audience/issued_at/...），
//!   通过 `#[serde(rename = "...")]` 桥接 RFC 7519 短码。
//!
//! ## 2026-09-23 重构要点
//! - session key 由 `sha256(token)` 改为 JWT 自带 jti（UUID v4）：
//!   Redis 主条目 key 现在是 `session:tok:<jti>`，`SessionStore` 入参从 `token_hash`
//!   改为 `jti`，`hash_token` 函数被删除（`sha2` crate 因 part_file 仍保留——见 Cargo.toml 注释）。
//! - `AuthenticatedTokenHash` 重命名为 `SessionJti`，值类型仍为 `String`，但语义
//!   从 sha256 hex 改为 jti UUID v4。
//!
//! ## 2026-09-23 重构要点：refresh rotation + reuse detection
//! - `verify_session_token` 在 `decode_access` 拿到 jti 后、`get_session` 之前
//!   新增黑名单 EXISTS 闸：`is_jti_revoked(&jti)` 为 true 即返回 40105 SESSION_REVOKED。
//! - 这是 refresh 轮转流程的入口闸——任何被轮转过的 access jti 在黑名单 TTL
//!   期间内被任意请求撞上即视为会话已失效，前端应清除本地 token 并跳回登录页。
//! - refresh 路径在 `iam::service::session::refresh` phase 1 还有第二道闸：
//!   撞上 refresh jti 黑名单视为 reuse detection 命中，触发
//!   `delete_all_user_sessions` 强制下线该用户所有 session（ACCOUNT_SECURITY_EVENT）。

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
/// 2. `is_jti_revoked(&jti)` 查黑名单（2026-09-23 新增）—— refresh rotation 入口闸；
///    命中即返回 40105，跳过 Redis 主条目查询
/// 3. 查 Redis `session:tok:<jti>`；查不到 / user_id 不匹配 → 40105；通过则继续
/// 4. `touch_session` 滑动 TTL；失败仅 warn
/// 5. roles 走 `parse_role_string` 把缓存里的大写字符串转回 `Role` enum（未知值 warn+skip）
/// 6. username/roles/shelf_ids/shelf_wildcard 从 `cached.profile.{...}` 取（注意字段名是
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
    //
    // 2026-09-23 重构：decode_access 走 RS256 + kid 路由，HS256 仅作 fallback。
    // 第二个参数 `public_keys` 来自 JwtConfig.public_keys（启动期目录扫描填充），
    // 第三个参数传 Some(&secret) —— jwt.rs 内部按 JwtConfig::allow_hs256_fallback
    // 决定是否启用 HS256 fallback 分支（caller 始终传 Some，由 jwt.rs 据 header.alg
    // 分支判断）。这样 call site 不需要在 middleware 重复读 config。
    let claims = decode_access(
        token,
        &state.config.jwt.public_keys,
        Some(&state.config.jwt.secret),
        &state.config.jwt.issuer,
        &state.config.jwt.audience,
    )?;

    // 2026-09-23 重构：session key 直接用 JWT 自带 jti（claims.jwt_id），
    // 不再对 token 做 sha256 哈希。
    let jti = claims.jwt_id.clone();

    // 2) 2026-09-23 重构：refresh rotation reuse detection 黑名单闸。
    // 黑名单命中 → 40105 SESSION_REVOKED，跳过 Redis 主条目查询。
    // 此闸是 access 路径的；refresh 路径在 `iam::service::session::refresh` phase 1 另有
    // 第二道闸（命中即触发 force_logout + ACCOUNT_SECURITY_EVENT）。
    if state.session.is_jti_revoked(&jti).await? {
        tracing::warn!(jti = %jti, "verify_session_token: access jti 在黑名单");
        return Err(AppError::biz(
            code::SESSION_REVOKED,
            "会话已被吊销，请重新登录",
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

    // 6) 从 cached.profile.{...} 拼装 CurrentUser（id/username/roles/shelf_ids/
    //    shelf_wildcard）；注意字段名是 `profile`，不是 `cached`（2026-09-22 重命名）。
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
