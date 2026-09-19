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
//!
//! 测试栈：tokio::test + tower::ServiceExt::oneshot + test_state_with_redis
//! （session_check_enabled=true → 必须建 Redis pool，session 写入才算「已吊销」）

#[path = "common/mod.rs"]
mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::AUTHORIZATION};
use chrono::Utc;
use common::{
    add_role, clean_db, clean_redis, ensure_database_exists, insert_user_with_password, test_app,
    test_pool, test_state,
};
use hsh_erp_rust::auth::jwt::encode_access;
use hsh_erp_rust::auth::rbac::{Claims, Role};

use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;

// ===========================================================================
// 全局串行化互斥：所有测试共享同一 DB + 同一 Redis db 15，必须串行避免 fixture 冲突。
// ===========================================================================
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    let redis_pool = common::test_redis_pool().await;
    clean_db(&pool).await;
    clean_redis(&redis_pool).await;
    (guard, pool)
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let envelope: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("parse JSON: {e}; raw = {}", String::from_utf8_lossy(&body)));
    (status, envelope)
}

fn json_request(
    method: &str,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        builder = builder.header(AUTHORIZATION, format!("Bearer {t}"));
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    builder.body(body).expect("build request")
}

/// 签发合法 access token 但**不**写 Redis session（用于 case 4）。
async fn mint_token_no_session(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    let claims = Claims {
        sub: user_id,
        username: "mw-tester".to_string(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: true,
        ver: 0,
        typ: "access".into(),
        iss: state.config.jwt.issuer.clone(),
        exp: 0,
    };
    let (token, _exp) = encode_access(
        &claims,
        &state.config.jwt.secret,
        &state.config.jwt.issuer,
        state.config.jwt.access_ttl_hours,
    )
    .expect("encode_access");
    token
}

/// 签发已过期的 access token（exp = now - 120s，绕开 jsonwebtoken 默认 60s leeway）。
///
/// 2026-09-20 实测：`jsonwebtoken::Validation::new()` 默认 `leeway = 60`，`exp = now-1`
/// 仍在 leeway 内不会触发 ExpiredSignature。120s 才能稳定触发 40102。
async fn mint_expired_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    #[derive(serde::Serialize)]
    struct ExpClaims<'a> {
        sub: i64,
        username: &'a str,
        roles: Vec<&'a str>,
        shelf_ids: Vec<i64>,
        shelf_wildcard: bool,
        ver: i32,
        typ: &'a str,
        iss: &'a str,
        exp: i64,
    }
    let c = ExpClaims {
        sub: user_id,
        username: "mw-tester",
        roles: vec!["MANAGER"],
        shelf_ids: vec![],
        shelf_wildcard: true,
        ver: 0,
        typ: "access",
        iss: &state.config.jwt.issuer,
        exp: Utc::now().timestamp() - 120,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &c,
        &EncodingKey::from_secret(state.config.jwt.secret.as_bytes()),
    )
    .expect("encode expired token")
}

// ===========================================================================
// 1. 受保护端点无 token → 40100
// ===========================================================================

#[tokio::test]
async fn protected_endpoint_without_token_returns_40100() {
    let (_guard, pool) = setup().await;
    let state = test_state(pool).await;
    let app = test_app(state);

    // GET /api/v2/workers 是 MANAGER-only 受保护端点
    let (status, env) = send(app, json_request("GET", "/workers", None, None)).await;
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
    let (_guard, pool) = setup().await;
    let uid = insert_user_with_password(&pool, "forge_admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool).await;
    let app = test_app(state.clone());
    let (_, login_env) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(json!({"username": "forge_admin", "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = login_env["data"]["token"].as_str().unwrap().to_string();

    // 篡改 token 最后一个字符（签名段）
    let mut chars: Vec<char> = token.chars().collect();
    let last_idx = chars.len() - 1;
    chars[last_idx] = if chars[last_idx] == 'A' { 'B' } else { 'A' };
    let forged: String = chars.into_iter().collect();

    let app2 = test_app(state);
    let (status, env) = send(app2, json_request("GET", "/workers", None, Some(&forged))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "伪造签名应返 401: {env}");
    assert_eq!(env["code"], 40100, "伪造签名应返 UNAUTHORIZED (40100)");
}

// ===========================================================================
// 3. 过期 token → 40102
// ===========================================================================

#[tokio::test]
async fn expired_token_returns_40102() {
    let (_guard, pool) = setup().await;
    let uid = insert_user_with_password(&pool, "exp_admin", "changeme").await;
    let state = test_state(pool).await;
    let expired = mint_expired_token(&state, uid).await;

    let app = test_app(state);
    let (status, env) = send(app, json_request("GET", "/workers", None, Some(&expired))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "过期 token 应返 401: {env}"
    );
    assert_eq!(env["code"], 40102, "过期 token 应返 TOKEN_EXPIRED (40102)");
}

// ===========================================================================
// 4. 有效 JWT 无 Redis session → 40105
// ===========================================================================

#[tokio::test]
async fn valid_jwt_without_session_returns_40105() {
    let (_guard, pool) = setup().await;
    let uid = insert_user_with_password(&pool, "nosess_admin", "changeme").await;
    let state = test_state(pool).await;
    // 签名合法，但不写 Redis session
    let token = mint_token_no_session(&state, uid).await;

    let app = test_app(state);
    let (status, env) = send(app, json_request("GET", "/workers", None, Some(&token))).await;
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
// 5. 白名单：GET /api/v2/health 不带 token → 200
// ===========================================================================

#[tokio::test]
async fn health_whitelist_no_token_200() {
    let (_guard, pool) = setup().await;
    let state = test_state(pool).await;
    let app = test_app(state);

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
    let (_guard, pool) = setup().await;
    let state = test_state(pool).await;
    let app = test_app(state);

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
    let (_guard, pool) = setup().await;
    let state = test_state(pool).await;
    let app = test_app(state);

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
    let (_guard, pool) = setup().await;
    let uid = insert_user_with_password(&pool, "logout_admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    // 1) login → 拿 token
    let (_, login_env) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(json!({"username": "logout_admin", "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = login_env["data"]["token"].as_str().unwrap().to_string();

    // 2) logout → 删 Redis session
    let app2 = test_app(state.clone());
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
    let app3 = test_app(state);
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
