//! `/api/v2/_e2e/*` seed hook 集成测试 —— 2026-09-14 新增
//!
//! ## 覆盖
//! 1. `probe` → 200 + enabled=true
//! 2. `reset` → 清 t_e2e_seeded 元数据行（业务表不受影响）
//! 3. `seed_customer_l1_then_l2` → 创建 L1 (带 serial_prefix) + L2 (带 parent_id)
//! 4. `seed_user_with_roles` → 创建 user + 多 role 行
//! 5. `revoke_session` → 写一条 session，再 revoke，确认 Redis set 清空
//! 6. `e2e_guard_disabled` → 把 state.config.enable_e2e_hooks 改 false，probe → 404
//!
//! ## 并行
//! 与 customer_api.rs 共享 `postgres_rust_test` 库，用进程级
//! `tokio::sync::Mutex` 串行化（避免 t_user / t_customer 唯一索引互相影响）。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use deadpool_redis::redis::AsyncCommands;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    clean_business_db, clean_db, clean_redis, ensure_database_exists, insert_user_with_password,
    test_app, test_pool, test_redis_pool, test_state_with_redis,
};
use hsh_erp_rust::auth::session::{CachedCurrentUser, RedisSessionStore, SessionStore, TokenKind};

// ===========================================================================
//  全局串行化 + helpers
// ===========================================================================
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body_str = String::from_utf8_lossy(&body).to_string();
    let envelope: Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!("parse JSON: {e}; status={status} raw = {body_str:?}")
    });
    (status, envelope)
}

fn json_request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    builder.body(body).expect("build request")
}

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool, deadpool_redis::Pool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    let redis = test_redis_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    clean_redis(&redis).await;
    (guard, pool, redis)
}

// ===========================================================================
//  1) probe
// ===========================================================================

#[tokio::test]
async fn probe_returns_ok_when_enabled() {
    let (_guard, pool, redis) = setup().await;
    let state = test_state_with_redis(pool.clone(), redis.clone());
    let app = test_app(state);

    let (status, env) = send(app, json_request("POST", "/_e2e/probe", None)).await;

    assert_eq!(status, StatusCode::OK, "probe: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "ok");
    assert_eq!(env["data"]["enabled"], true);
}

// ===========================================================================
//  2) reset：清 t_e2e_seeded 行
// ===========================================================================

#[tokio::test]
async fn reset_clears_seeded_metadata_only() {
    let (_guard, pool, redis) = setup().await;
    let state = test_state_with_redis(pool.clone(), redis.clone());
    let app = test_app(state.clone());

    // seed 一个 customer（自动写入 t_e2e_seeded 一行）
    let (_, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/_e2e/seed/customer",
            Some(json!({"name": "ResetTest-L1", "serial_prefix": "R"})),
        ),
    )
    .await;
    assert_eq!(env["code"], 0);

    // 确认 t_e2e_seeded 至少有一行
    let pre_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t_e2e_seeded")
        .fetch_one(&pool)
        .await
        .expect("count t_e2e_seeded before reset");
    assert!(pre_count >= 1, "expected at least 1 row before reset");

    // reset
    let (status, env) = send(app.clone(), json_request("POST", "/_e2e/reset", None)).await;
    assert_eq!(status, StatusCode::OK, "reset: {env}");
    assert_eq!(env["code"], 0);
    assert!(
        env["data"]["cleared"].as_i64().unwrap() >= 1,
        "expected cleared >= 1; got {env}"
    );

    // t_e2e_seeded 应该清空
    let post_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t_e2e_seeded")
        .fetch_one(&pool)
        .await
        .expect("count t_e2e_seeded after reset");
    assert_eq!(post_count, 0, "t_e2e_seeded should be empty after reset");

    // 业务表 customer 仍在（reset 仅清元数据）
    let cust_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t_customer")
        .fetch_one(&pool)
        .await
        .expect("count t_customer after reset");
    assert_eq!(
        cust_count, 1,
        "t_customer should still have 1 row after reset"
    );
}

// ===========================================================================
//  3) seed/customer L1 + L2
// ===========================================================================

#[tokio::test]
async fn seed_customer_l1_then_l2() {
    let (_guard, pool, redis) = setup().await;
    let state = test_state_with_redis(pool.clone(), redis.clone());
    let app = test_app(state);

    // L1
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/_e2e/seed/customer",
            Some(json!({"name": "ACME-L1", "serial_prefix": "A"})),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "seed L1: {env1}");
    assert_eq!(env1["code"], 0);
    let l1_id = env1["data"]["id"].as_str().expect("L1 id string").to_string();
    assert!(
        l1_id.parse::<i64>().is_ok(),
        "L1 id should be parseable as i64"
    );

    // 在 t_customer 查到 L1
    let l1_back: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT name, serial_prefix FROM t_customer WHERE id = $1",
    )
    .bind(l1_id.parse::<i64>().unwrap())
    .fetch_optional(&pool)
    .await
    .expect("query L1 back");
    let (name, prefix) = l1_back.expect("L1 should exist in t_customer");
    assert_eq!(name, "ACME-L1");
    assert_eq!(prefix.as_deref(), Some("A"));

    // L2
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/_e2e/seed/customer",
            Some(json!({
                "name": "ACME-L2",
                "parent_id": l1_id.clone(),
            })),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "seed L2: {env2}");
    assert_eq!(env2["code"], 0);
    let l2_id = env2["data"]["id"].as_str().expect("L2 id string").to_string();

    // 确认 parent_id 在 DB 里关联正确
    let l2_parent: Option<i64> =
        sqlx::query_scalar("SELECT parent_id FROM t_customer WHERE id = $1")
            .bind(l2_id.parse::<i64>().unwrap())
            .fetch_one(&pool)
            .await
            .expect("query L2 parent");
    assert_eq!(l2_parent, Some(l1_id.parse::<i64>().unwrap()));
}

// ===========================================================================
//  4) seed/user 多角色
// ===========================================================================

#[tokio::test]
async fn seed_user_with_roles_inserts_role_rows() {
    let (_guard, pool, redis) = setup().await;
    let state = test_state_with_redis(pool.clone(), redis.clone());
    let app = test_app(state);

    let (status, env) = send(
        app,
        json_request(
            "POST",
            "/_e2e/seed/user",
            Some(json!({
                "username": "e2e_user_a",
                "role_codes": ["MANAGER", "CLERK"],
                "full_name": "E2E User A",
                "phone": "13800000001",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed user: {env}");
    assert_eq!(env["code"], 0);
    let uid: i64 = env["data"]["id"]
        .as_str()
        .expect("user id string")
        .parse()
        .expect("parse user id as i64");

    // 确认 t_user 有一行 + is_active=true
    let user_row: Option<(String, bool)> =
        sqlx::query_as("SELECT username, is_active FROM t_user WHERE id = $1")
            .bind(uid)
            .fetch_optional(&pool)
            .await
            .expect("query t_user");
    let (uname, active) = user_row.expect("user should exist");
    assert_eq!(uname, "e2e_user_a");
    assert!(active, "user should be active");

    // t_user_role 应该有 2 行
    let role_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t_user_role WHERE user_id = $1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .expect("count roles");
    assert_eq!(role_count, 2, "expected 2 t_user_role rows");

    let roles: Vec<String> =
        sqlx::query_scalar("SELECT role FROM t_user_role WHERE user_id = $1 ORDER BY role")
            .bind(uid)
            .fetch_all(&pool)
            .await
            .expect("select roles");
    assert_eq!(roles, vec!["CLERK", "MANAGER"]);
}

// ===========================================================================
//  5) revoke-session：写一条 session 后 revoke，确认 set 清空
// ===========================================================================

#[tokio::test]
async fn revoke_session_clears_redis_user_set() {
    let (_guard, pool, redis) = setup().await;
    let state = test_state_with_redis(pool.clone(), redis.clone());
    let app = test_app(state.clone());

    // seed 一个 user
    let (status, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/_e2e/seed/user",
            Some(json!({
                "username": "revoke_target",
                "role_codes": ["MANAGER"],
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed user: {env}");
    let uid: i64 = env["data"]["id"].as_str().unwrap().parse().unwrap();

    // 模拟登录：写一条 session 到 Redis
    let store = RedisSessionStore::new(redis.clone());
    let token_hash = "fakehash-revoke-session-test";
    store
        .create_session(
            token_hash,
            uid,
            TokenKind::Access,
            3600,
            &CachedCurrentUser {
                id: uid,
                username: "revoke_target".into(),
                roles: vec!["MANAGER".into()],
                shelf_ids: vec![],
                shelf_wildcard: false,
            },
        )
        .await
        .expect("create_session");

    // 确认 set 有该 token_hash
    let set_key = format!("sessions:user:{uid}");
    let mut conn = redis.get().await.expect("redis conn");
    let members_before: Vec<String> = conn
        .smembers(&set_key)
        .await
        .expect("smembers before");
    assert!(
        members_before.contains(&token_hash.to_string()),
        "expected token_hash in user set; got {members_before:?}"
    );

    // revoke-session
    let (rs, renv) = send(
        app,
        json_request(
            "POST",
            "/_e2e/revoke-session",
            Some(json!({"username": "revoke_target"})),
        ),
    )
    .await;
    assert_eq!(rs, StatusCode::OK, "revoke: {renv}");
    assert_eq!(renv["code"], 0);

    // Redis set 应该被清空
    let members_after: Vec<String> = conn
        .smembers(&set_key)
        .await
        .expect("smembers after");
    assert!(
        members_after.is_empty(),
        "expected empty user set after revoke; got {members_after:?}"
    );
}

// ===========================================================================
//  6) e2e_guard disabled → 404
// ===========================================================================

#[tokio::test]
async fn e2e_guard_returns_404_when_disabled() {
    let (_guard, pool, redis) = setup().await;
    let state = test_state_with_redis(pool.clone(), redis.clone());

    // 临时改 config 关掉 hook —— 需要独占可变访问（Arc<AppConfig> 是只读）
    // 直接修改 AppConfig 不可行（Arc 内层用 Arc::make_mut）
    let mut new_config: AppConfig = (*state.config).clone();
    new_config.enable_e2e_hooks = false;
    let new_state = std::sync::Arc::new(hsh_erp_rust::state::AppState::new(
        state.pool.clone(),
        std::sync::Arc::new(new_config),
        state.snowflake.clone(),
        state.ws_hub.clone(),
        state.cos.clone(),
        state.shutdown.clone(),
        state.session.clone(),
    ));

    let app = test_app(new_state);
    let (status, env) = send(app, json_request("POST", "/_e2e/probe", None)).await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "probe with hooks disabled should 404; got env: {env}"
    );
    assert_eq!(env["code"], 40400);

    // 别忘了确保静态类型 AppConfig 在测试文件里被导入（避免未使用警告）
    let _ = insert_user_with_password;
}

// ===========================================================================
//  类型导入集中区 —— 避免上面散落 noise
// ===========================================================================
use hsh_erp_rust::infra::config::AppConfig;
