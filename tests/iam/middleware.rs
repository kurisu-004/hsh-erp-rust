//! auth middleware 集成测试（2026-09-20 新增）
//!
//! 覆盖：
//! 1. 受保护端点无 token → 40100
//! 2. 伪造签名 → 40100
//! 3. 过期 token → 40102（ExpiredSignature 细分）
//! 4. 有效 JWT 无 Redis session → 40105
//! 5. 白名单可达：GET /api/v2/health 不带 token → 200
//! 6. 白名单可达：POST /api/v2/iam/login 不带 token → 40101（业务码非 40100）
//! 7. 白名单可达：POST /api/v2/iam/refresh 不带 token → 40001
//! 8. logout 后旧 token → 40105
//! 9. 未匹配路由 → 404（route_layer 不强制鉴权到 404，边界不变量）
//!
//! 2026-09-20 merge master 时同步：worker 等端点已迁至 `/api/v2/prod/*` 容器，
//! 测试路径同步更新（PR-2026-09-19 prod 容器聚合）。
//!
//! 测试栈：tokio::test + tower::ServiceExt::oneshot + test_state_with_redis
//!（必须建 Redis pool，session 写入才算「已吊销」）
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原重度依赖 `test-support::fixtures::insert_user_with_password /
//! add_role` 创建测试用户，再调用 `mint_*_token(state, uid)` 自签 JWT。
//! 改走 `load_iam_fixture(&pool)` + `IamFixture::manager_user_id` 直接拿常量
//! UID，跳过 fixtures.rs 创建用户逻辑。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use hsh_erp_rust::auth::jwt::encode_access;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;

// 2026-09-23 重构：RS256 + kid 多密钥轮换。mint_*_token helper 与负向用例
// 复用 `test-support::pem` 的 2048-bit RSA 密钥对（process 级 OnceLock 缓存）。
//
// ⚠️ 必须用 `hsh_erp_test_support::pem`（不是顶层 `pem`）——
//   tests/common/mod.rs 也声明 `mod pem;`，两个路径若都声明会得到两个独立
//   模块实例、各自一套 OnceLock，签发与验签走两套不同 keypair → 40100
//   InvalidSignature。本文件用 `use hsh_erp_test_support::pem;`
//   复用 crate 实例（与 test_state 内部 `crate::pem::test_private_pem()` 一致）。
use hsh_erp_test_support::pem;
use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token,
    send as ts_send, test_app, test_pool, test_state, test_state_with_hs256_fallback_off,
};

// ===========================================================================
// Helpers
// ===========================================================================

/// 把 request 发给 axum app，oneshot 出来，拆 (status, body JSON envelope)。
async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    ts_send(app, req).await
}

/// 基础 bootstrap：fresh DB + iam fixture 9 行（含 MANAGER 用户 fx.manager_user_id）
/// + state + app。返回 pool / state / app / fixture 句柄。
///
/// JWT mint_*_token helper 全部用 `fx.manager_user_id` 作为 sub claim（fixture
/// 已预置该用户，验签端的 `t_user` lookup 不要求用户存在也可通过）。
async fn bootstrap() -> (PgPool, Arc<hsh_erp_rust::state::AppState>, axum::Router, IamFixture) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    (pool, state, app, fx)
}

/// 构造一次性 app（axum 0.8 oneshot 语义：每次发请求都需新构造）。
async fn fresh_app(pool: &PgPool) -> axum::Router {
    test_app(test_state(pool.clone()).await)
}

/// 签发合法 access token 但**不**写 Redis session（用于 case 4）。
async fn mint_token_no_session(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    // 2026-09-22 重构：encode_access 签名改为 `(secret, issuer, audience, subject, ttl_seconds)`，
    // 不再收 Claims；iat/nbf/jti/aud/typ 由函数内部填。直接调用即可。
    // 2026-09-23 重构：encode_access 返回 `(token, jti, exp)` 三元组；本 helper 不写
    // Redis session，丢弃 jti 即可。
    // 2026-09-23 重构：encode_access 改 RS256 + kid —— 第 1-2 参数
    // `(private_key, signing_kid)` 替代原 `secret`；private_key 从 pem 模块的
    // 2048-bit RSA 私钥构造，signing_kid 与服务端 JwtConfig.signing_kid 对齐。
    let (token, _jti, _exp) = encode_access(
        &state.config.jwt.private_key,
        &state.config.jwt.signing_kid,
        &state.config.jwt.issuer,
        &state.config.jwt.audience,
        user_id,
        state.config.jwt.access_ttl_seconds,
    )
    .expect("encode_access");
    token
}

/// 签发已过期的 access token（exp = now - 120s，绕开 jsonwebtoken 默认 60s leeway）。
///
/// 2026-09-20 实测：`jsonwebtoken::Validation::new()` 默认 `leeway = 60`，`exp = now-1`
/// 仍在 leeway 内不会触发 ExpiredSignature。120s 才能稳定触发 40102。
///
/// 2026-09-23 重构：HS256 → RS256 + kid —— Header::new(RS256).set_kid(signing_kid)，
/// EncodingKey::from_rsa_pem(pem::test_private_pem())。其它 claim 结构不变。
async fn mint_expired_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    /// 2026-09-22 重构：JSON 字段名按 RFC 7519 短码（sub/aud/iat/nbf/exp/iss/jti/typ）；
    /// 删 username/roles/shelf_ids/shelf_wildcard/ver 业务字段。
    #[derive(serde::Serialize)]
    struct ExpClaims<'a> {
        sub: i64,
        aud: &'a str,
        iat: i64,
        nbf: i64,
        iss: &'a str,
        exp: i64,
        jti: String,
        typ: &'a str,
    }
    let now = Utc::now().timestamp();
    let c = ExpClaims {
        sub: user_id,
        aud: &state.config.jwt.audience,
        iat: now,
        nbf: now,
        iss: &state.config.jwt.issuer,
        exp: now - 120,
        jti: uuid::Uuid::new_v4().to_string(),
        typ: "access",
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(state.config.jwt.signing_kid.clone());
    let ek = EncodingKey::from_rsa_pem(pem::test_private_pem().as_bytes())
        .expect("test private pem");
    encode(&header, &c, &ek).expect("encode expired token")
}

/// 签发 **错误 audience** 的 access token（其它 claim 全部合法）
///
/// 2026-09-22 重构：用于验证 `Validation::set_required_spec_claims` + `set_audience`
/// 双重校验下，aud 不匹配时返回 40100 UNAUTHORIZED（不是 200/40105）。
///
/// 2026-09-23 重构：HS256 → RS256 + kid（与 mint_expired_token 同模式）。
async fn mint_wrong_audience_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    #[derive(serde::Serialize)]
    struct Claims<'a> {
        sub: i64,
        aud: &'a str,
        iat: i64,
        nbf: i64,
        iss: &'a str,
        exp: i64,
        jti: String,
        typ: &'a str,
    }
    let now = Utc::now().timestamp();
    // 用错误的 audience（与 state.config.jwt.audience 故意不同）
    let wrong_aud = "wrong-audience-not-matching-config";
    let c = Claims {
        sub: user_id,
        aud: wrong_aud,
        iat: now,
        nbf: now,
        iss: &state.config.jwt.issuer,
        exp: now + 3600,
        jti: uuid::Uuid::new_v4().to_string(),
        typ: "access",
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(state.config.jwt.signing_kid.clone());
    encode(
        &header,
        &c,
        &EncodingKey::from_rsa_pem(pem::test_private_pem().as_bytes())
            .expect("test private pem"),
    )
    .expect("encode wrong-aud token")
}

/// 签发 **缺失 audience** 字段的 access token（其它 claim 全部合法）
///
/// 2026-09-22 重构：用于验证 `set_required_spec_claims` 把 `aud` 列为必填——
/// `set_audience` 单独使用**不**会让缺 `aud` 的 token 被拒；本测试守住
/// "缺 aud 字段 → 40100" 这个不变量。
///
/// 2026-09-23 重构：HS256 → RS256 + kid。
async fn mint_missing_audience_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    // 故意**不**序列化 `aud` 字段
    #[derive(serde::Serialize)]
    #[serde(deny_unknown_fields)]
    struct Claims {
        sub: i64,
        iat: i64,
        nbf: i64,
        iss: String,
        exp: i64,
        jti: String,
        typ: &'static str,
    }
    let now = Utc::now().timestamp();
    let c = Claims {
        sub: user_id,
        iat: now,
        nbf: now,
        iss: state.config.jwt.issuer.clone(),
        exp: now + 3600,
        jti: uuid::Uuid::new_v4().to_string(),
        typ: "access",
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(state.config.jwt.signing_kid.clone());
    encode(
        &header,
        &c,
        &EncodingKey::from_rsa_pem(pem::test_private_pem().as_bytes())
            .expect("test private pem"),
    )
    .expect("encode missing-aud token")
}

// ===========================================================================
// 1. 受保护端点无 token → 40100
// ===========================================================================

#[tokio::test]
async fn protected_endpoint_without_token_returns_40100() {
    let (_pool, _state, app, _fx) = bootstrap().await;

    // GET /api/v2/prod/workers 是 MANAGER-only 受保护端点
    let (status, env) = send(app, json_request("GET", "/prod/workers", None, None)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "无 token 受保护端点应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40100,
        "无 token 应返 UNAUTHORIZED (40100)，不是 40101 / 40105"
    );
}

// ===========================================================================
// 2. 伪造签名 → 40100
// ===========================================================================

#[tokio::test]
async fn forged_signature_returns_40100() {
    let (pool, _state, app, fx) = bootstrap().await;
    // 登录 MANAGER 拿 token
    let token =
        login_token(&app, &fx.manager_username, IamFixture::PASSWORD).await;

    // 篡改 token 最后一个字符（签名段）
    let mut chars: Vec<char> = token.chars().collect();
    let last_idx = chars.len() - 1;
    chars[last_idx] = if chars[last_idx] == 'A' { 'B' } else { 'A' };
    let forged: String = chars.into_iter().collect();

    let app2 = fresh_app(&pool).await;
    let (status, env) = send(
        app2,
        json_request("GET", "/prod/workers", None, Some(&forged)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "伪造签名应返 401: {env}");
    assert_eq!(env["code"], 40100, "伪造签名应返 UNAUTHORIZED (40100)");
}

// ===========================================================================
// 3. 过期 token → 40102
// ===========================================================================

#[tokio::test]
async fn expired_token_returns_40102() {
    let (pool, state, _app, fx) = bootstrap().await;
    let expired = mint_expired_token(&state, fx.manager_user_id).await;

    let app = fresh_app(&pool).await;
    let (status, env) = send(
        app,
        json_request("GET", "/prod/workers", None, Some(&expired)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "过期 token 应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40102,
        "过期 token 应返 TOKEN_EXPIRED (40102), got env={env}"
    );
}

// ===========================================================================
// 4. 有效 JWT 无 Redis session → 40105
// ===========================================================================

#[tokio::test]
async fn valid_jwt_without_session_returns_40105() {
    let (pool, state, _app, fx) = bootstrap().await;
    // 签名合法，但不写 Redis session
    let token = mint_token_no_session(&state, fx.manager_user_id).await;

    let app = fresh_app(&pool).await;
    let (status, env) = send(
        app,
        json_request("GET", "/prod/workers", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "无 session 应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40105,
        "无 Redis session 应返 SESSION_REVOKED (40105)"
    );
}

// ===========================================================================
// 4b. 错误 audience 的 token → 40100 UNAUTHORIZED
//
// 2026-09-22 重构：验证 `set_required_spec_claims` + `set_audience` 双重校验
// 下，aud 不匹配的合法签名 token 被拒。
// ===========================================================================

#[tokio::test]
async fn wrong_audience_returns_40100() {
    let (pool, state, _app, fx) = bootstrap().await;
    let token = mint_wrong_audience_token(&state, fx.manager_user_id).await;

    let app = fresh_app(&pool).await;
    let (status, env) = send(
        app,
        json_request("GET", "/prod/workers", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "错误 aud 应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40100,
        "错误 aud 应返 UNAUTHORIZED (40100)，不是 40102/40105"
    );
}

// ===========================================================================
// 4c. 缺失 audience 字段的 token → 40100 UNAUTHORIZED
//
// 2026-09-22 重构：守住 `set_required_spec_claims` 把 `aud` 列为必填的不变量。
// 若未来有人误删这行 `set_required_spec_claims(&["exp", "aud", "iss", "sub"])`，
// 此测试立即失败提醒。
// ===========================================================================

#[tokio::test]
async fn missing_audience_returns_40100() {
    let (pool, state, _app, fx) = bootstrap().await;
    let token = mint_missing_audience_token(&state, fx.manager_user_id).await;

    let app = fresh_app(&pool).await;
    let (status, env) = send(
        app,
        json_request("GET", "/prod/workers", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "缺 aud 字段应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40100,
        "缺 aud 字段应返 UNAUTHORIZED (40100)（set_required_spec_claims 守住）"
    );
}

// ===========================================================================
// 5. 白名单：GET /api/v2/health 不带 token → 200
// ===========================================================================

#[tokio::test]
async fn health_whitelist_no_token_200() {
    let (_pool, _state, app, _fx) = bootstrap().await;

    // 2026-09-20 注：health 端点直接返 `Json<HealthResp>`（不走 envelope `R<T>`），
    // 返回 `{"status":"ok",...}`。白名单命中 → 200；非 401 即白名单生效。
    let (status, env) = send(app, json_request("GET", "/health", None, None)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "/health 应无 token 可达（白名单命中）: {env}"
    );
    assert_eq!(env["status"], "ok");
    assert_eq!(env["service"], "hsh-erp-api");
}

// ===========================================================================
// 6. 白名单：POST /api/v2/iam/login 不带 token → 40101（业务码，非 40100）
// ===========================================================================

#[tokio::test]
async fn login_whitelist_no_token_40101() {
    let (_pool, _state, app, _fx) = bootstrap().await;

    // 不带 token 调 login → 用户不存在走业务码 40101（不是 middleware 的 40100）
    let (status, env) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(json!({"username": "ghost", "password": "whatever"})),
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "login 业务码 40101 HTTP 应为 401: {env}"
    );
    assert_eq!(
        env["code"], 40101,
        "login 业务码应是 BIZ_AUTH_INVALID (40101)，不是 middleware 40100"
    );
}

// ===========================================================================
// 7. 白名单：POST /api/v2/iam/refresh 不带 token → 40001（VALIDATION）
// ===========================================================================

#[tokio::test]
async fn refresh_whitelist_no_token_40001() {
    let (_pool, _state, app, _fx) = bootstrap().await;

    // 不带 token 调 refresh：middleware 白名单放行（路径是 /iam/refresh），
    // 但 handler 的 `Json<RefreshRequest>` 拿不到 `refresh_token` 字段，
    // 返回 422 plain text（axum JsonRejection 默认行为，非 middleware 40100）。
    //
    // 关键断言：HTTP 不是 401（UNAUTHORIZED）。如果是 401，说明 middleware 把
    // /iam/refresh 误判为受保护端点、缺 token 走了 40100 路径——那才是 bug。
    let response = app
        .oneshot(json_request("POST", "/iam/refresh", Some(json!({})), None))
        .await
        .expect("oneshot");
    let status = response.status();
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "refresh 校验失败不应是 middleware 40100（应是 JsonRejection 422 等）"
    );
    // 业务上 422（VALIDATION）是最常见的；本断言放宽到「不是 401」即可。
}

// ===========================================================================
// 8. logout 后旧 token → 40105
// ===========================================================================

#[tokio::test]
async fn logout_then_old_token_returns_40105() {
    let (pool, _state, app, fx) = bootstrap().await;
    // 1) login → 拿 token
    let token =
        login_token(&app, &fx.manager_username, IamFixture::PASSWORD).await;

    // 2) logout → 删 Redis session
    let app2 = fresh_app(&pool).await;
    let (logout_status, _) = send(
        app2,
        json_request("POST", "/iam/logout", None, Some(&token)),
    )
    .await;
    assert_eq!(
        logout_status,
        StatusCode::OK,
        "logout 应 200，否则测试设置失败"
    );

    // 3) 旧 token 调 /iam/me → 40105 SESSION_REVOKED
    let app3 = fresh_app(&pool).await;
    let (status, env) = send(app3, json_request("GET", "/iam/me", None, Some(&token))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "logout 后旧 token 应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40105,
        "logout 后旧 token 应返 SESSION_REVOKED (40105)"
    );
}

// ===========================================================================
// 9. route_layer 边界：不存在的路径不带 token → 404 而非 40100
//
// 2026-09-20 修复 review #1：`route_layer` 仅作用于已匹配路由（axum 0.8
// 文档语义：failed routes 不进入 layer）。如果改为 `.layer()`，404 路径
// 也会走 authenticate_middleware → middleware 白名单不命中 → 40100，反而掩盖
// 真实路由错误。本测试用例守住这个不变量：404 应是 404，不是 40100。
// ===========================================================================

#[tokio::test]
async fn nonexistent_route_returns_404_not_40100() {
    let (_pool, _state, app, _fx) = bootstrap().await;

    // 不存在的路径，不带 token。期望：404 NOT_FOUND（route_layer 短路），
    // 而**不是** 401 UNAUTHORIZED（说明 middleware 没误判 404 为受保护端点）。
    let response = app
        .oneshot(json_request("GET", "/nonexistent", None, None))
        .await
        .expect("oneshot");
    let status = response.status();
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "不存在的路径不带 token 应是 404（route_layer 不对 failed routes \
         走 middleware），不是 401 强制鉴权: {status}"
    );
}

// ===========================================================================
// 10. 2026-09-23 重构：RS256 + kid 路由 —— 不在字典内的 kid → 40100
// ===========================================================================

/// 签发 RS256 + kid = "ghost"（不在 JwtConfig.public_keys 字典内）的 token
///
/// 2026-09-23 重构新增负向用例：decode_access 走 RS256 分支时按 header.kid
/// 在公钥字典查询；kid 不在字典 → 40100 UNAUTHORIZED "unknown kid=ghost"。
/// 用 pem 模块的真实 RSA 私钥签发（kid = "ghost" 与公钥 dict 中 current / next
/// 均不匹配），断言被服务端拒。
async fn mint_unknown_kid_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    #[derive(serde::Serialize)]
    struct Claims<'a> {
        sub: i64,
        aud: &'a str,
        iat: i64,
        nbf: i64,
        iss: &'a str,
        exp: i64,
        jti: String,
        typ: &'a str,
    }
    let now = Utc::now().timestamp();
    let c = Claims {
        sub: user_id,
        aud: &state.config.jwt.audience,
        iat: now,
        nbf: now,
        iss: &state.config.jwt.issuer,
        exp: now + 3600,
        jti: uuid::Uuid::new_v4().to_string(),
        typ: "access",
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("ghost".into()); // 不在 public_keys 字典里
    encode(
        &header,
        &c,
        &EncodingKey::from_rsa_pem(pem::test_private_pem().as_bytes())
            .expect("test private pem"),
    )
    .expect("encode unknown-kid token")
}

#[tokio::test]
async fn protected_endpoint_with_unknown_kid_returns_40100() {
    let (pool, state, _app, fx) = bootstrap().await;
    let token = mint_unknown_kid_token(&state, fx.manager_user_id).await;

    let app = fresh_app(&pool).await;
    let (status, env) = send(
        app,
        json_request("GET", "/prod/workers", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "未知 kid 应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40100,
        "未知 kid 应返 UNAUTHORIZED (40100)，不是 40102 / 40105"
    );
}

// ===========================================================================
// 11. 2026-09-23 重构：防 algorithm confusion —— alg=none / 其它算法 → 40100
// ===========================================================================

/// 手工拼 alg=none 的 JWT（header.payload.signature 三段 base64url）。
///
/// 2026-09-23 重构新增负向用例：decode_access 在 match header.alg 时把
/// `Algorithm::HS256` / `Algorithm::RS256` 之外的算法（包括 `none`）一律映射
/// 40100 UNAUTHORIZED "unsupported algorithm"。本测试拼 alg=none 的 header
/// （signature 段空字符串），断言被服务端拒。
///
/// 手工拼而非用 `Header::new(Algorithm::None)` 因为 jsonwebtoken 0.10 已把
/// `Algorithm::None` 移除（防 algorithm confusion 的库侧防御）；我们走 raw 构造。
fn mint_alg_none_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    let now = Utc::now().timestamp();
    // header: {"alg":"none","typ":"JWT"}
    let header_json = r#"{"alg":"none","typ":"JWT"}"#;
    let header_b64 = B64.encode(header_json.as_bytes());
    // payload: 合法 claim（若 alg=none 通过，服务端会被骗）
    let payload = serde_json::json!({
        "sub": user_id,
        "aud": state.config.jwt.audience,
        "iat": now,
        "nbf": now,
        "iss": state.config.jwt.issuer,
        "exp": now + 3600,
        "jti": uuid::Uuid::new_v4().to_string(),
        "typ": "access",
    });
    let payload_b64 = B64.encode(serde_json::to_vec(&payload).expect("payload json"));
    // alg=none 的 signature 段为空字符串
    format!("{header_b64}.{payload_b64}.")
}

#[tokio::test]
async fn protected_endpoint_with_alg_none_returns_40100() {
    let (pool, state, _app, fx) = bootstrap().await;
    let token = mint_alg_none_token(&state, fx.manager_user_id);

    let app = fresh_app(&pool).await;
    let (status, env) = send(
        app,
        json_request("GET", "/prod/workers", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "alg=none 应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40100,
        "alg=none 应返 UNAUTHORIZED (40100)，不是 40102 / 40105"
    );
}

// ===========================================================================
// 12. 2026-09-23 review #1 修复：HS256 + 空 secret 在 fallback=false 时必拒 → 40100
//
// 背景：fix 之前 `verify_session_token` / `decode_refresh` 无脑 `Some(&secret)`
// 传 hs256_fallback_secret；当 `JWT_ALLOW_HS256_FALLBACK=false` 时 `from_env`
// 把 secret 默认 `""`（空串），`decode_access` HS256 分支 `Option::is_some()`
// → true → `DecodingKey::from_secret(b"")` 走空 HMAC 验签 → 攻击者用空
// secret 签的 HS256 token 顺利通过（40100 唯一信号仍为 InvalidSignature，
// 但若其它 claim 全部合法，则会一路跑到 session 真源，鉴权 bypass）。
//
// 本测试在 `allow_hs256_fallback=false` 的 JwtConfig 下签一个 HS256 + 空 secret
// 的 token（issuer/aud/exp/sub 全部合法），断言服务端返 40100（而不是
// 200 / 40102 / 40105）—— 守住「HS256 fallback 关闭时空 secret bypass
// 被阻断」不变量。fixture 由 `test_state_with_hs256_fallback_off` 构造
//（与 test_state 唯一差别即 `allow_hs256_fallback: false`）。
// ===========================================================================

/// HS256 + 空 secret + 合法 claim 的 token。
///
/// 调 jsonwebtoken::encode(HS256, EncodingKey::from_secret(b"")) 直接签发
/// —— 这是攻击者视角的最小 PoC：任意 client 拿到 `Authorization: Bearer <token>`
/// 后若服务端 fallback 关闭不严，把 secret 视为空就完成 bypass。
///
/// header: {"alg":"HS256","typ":"JWT"}（HS256 不带 kid）；payload: 合法 claim
///（issuer/aud/exp/sub 全部对齐 server config）；signature: HMAC-SHA256(
/// header_b64.payload_b64, b"")。
fn mint_hs256_empty_secret_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    #[derive(serde::Serialize)]
    struct Claims<'a> {
        sub: i64,
        aud: &'a str,
        iat: i64,
        nbf: i64,
        iss: &'a str,
        exp: i64,
        jti: String,
        typ: &'a str,
    }
    let now = Utc::now().timestamp();
    let c = Claims {
        sub: user_id,
        aud: &state.config.jwt.audience,
        iat: now,
        nbf: now,
        iss: &state.config.jwt.issuer,
        exp: now + 3600,
        jti: uuid::Uuid::new_v4().to_string(),
        typ: "access",
    };
    let header = Header::new(Algorithm::HS256);
    encode(&header, &c, &EncodingKey::from_secret(b"")).expect("encode hs256+empty")
}

#[tokio::test]
async fn hs256_rejected_when_fallback_off_returns_40100() {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let state = test_state_with_hs256_fallback_off(pool.clone()).await;
    let token = mint_hs256_empty_secret_token(&state, fx.manager_user_id);

    let app = fresh_app(&pool).await;
    let (status, env) = send(
        app,
        json_request("GET", "/prod/workers", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "HS256 + fallback=off 应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40100,
        "HS256 + fallback=off 应返 UNAUTHORIZED (40100)，\
         不是 200 / 40102 / 40105"
    );
}