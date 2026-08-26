//! worker 域端到端集成测试
//!
//! ## 覆盖（Task 4 worker CRUD + verify-badge）
//! 1. `verify_badge_inactive_returns_20202` — 工人存在但 `is_active=false` →
//!    `verify-badge` 端点返回 400 + 20202 `BIZ_WORKER_INACTIVE`。
//!
//! ## 并行
//! 所有用例共享 `postgres_rust_test` + `uk_t_worker_badge_code` 唯一约束，用
//! 进程级 `tokio::sync::Mutex` 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（写路径要求 M-only，按设计 §6.1 用 M 即可）。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, ensure_database_exists, insert_user_with_password,
    test_app, test_pool, test_state,
};

// ===========================================================================
// 全局串行化 + helpers（与 customer_api.rs / process_api.rs / shelf_api.rs 同形）
// ===========================================================================
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let uri = req.uri().to_string();
    let method = req.method().to_string();
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body_str = String::from_utf8_lossy(&body).to_string();
    let envelope: Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!(
            "parse JSON: {e}; method={method} uri={uri} status={status}; raw = {body_str:?}"
        )
    });
    (status, envelope)
}

fn json_request(method: &str, uri: &str, body: Option<Value>, bearer: Option<&str>) -> Request<Body> {
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

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": username, "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token)
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn verify_badge_inactive_returns_20202() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "worker_admin").await;

    // Create worker (active by default).
    let (s_create, env_create) = send(
        app.clone(),
        json_request(
            "POST",
            "/workers",
            Some(json!({"badge_code": "B001", "name": "Alice"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_create,
        StatusCode::CREATED,
        "create worker: {env_create}"
    );
    assert_eq!(env_create["code"], 0);
    let wid = env_create["data"]["id"].as_str().unwrap().to_string();

    // Deactivate (sets is_active=false + deleted_at=now()).
    let (s_deact, env_deact) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/workers/{wid}/deactivate"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_deact,
        StatusCode::OK,
        "deactivate worker: {env_deact}"
    );

    // verify-badge → 20202 (BIZ_WORKER_INACTIVE) with HTTP 400
    let (s_verify, env_verify) = send(
        app,
        json_request(
            "POST",
            "/workers/verify-badge",
            Some(json!({"badge_code": "B001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_verify,
        StatusCode::BAD_REQUEST,
        "verify-badge on inactive worker should return 400; got {env_verify}"
    );
    assert_eq!(
        env_verify["code"].as_i64().unwrap(),
        20202,
        "expected BIZ_WORKER_INACTIVE; got envelope: {env_verify}"
    );
}
