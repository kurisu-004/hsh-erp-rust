//! customer 域端到端集成测试
//!
//! ## 覆盖（Phase P1 customer CRUD 段）
//! 1. create L1（带 serial_prefix）+ create L2（带 parent_id）+ soft-delete L1 → 20113
//!    BIZ_CUSTOMER_IN_USE（因为 L1 仍被 t_part 引用）。
//!
//! ## 并行
//! 所有用例共享 `postgres_rust_test` + 唯一约束 `uq_t_customer_root_prefix`，用
//! 进程级 `tokio::sync::Mutex` 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（POST /customers 写路径要求 M/C，按设计 §6.1 用 M 即可）。

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
//  全局串行化 + helpers（与 delivery_group_api.rs 同形，按约定不跨文件复用）
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
        panic!(
            "parse JSON: {e}; status={status} uri=? raw = {body_str:?}"
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

/// 直插一个 `t_part` 行（customer_id = given），让 soft-delete 检查「被 part 引用」分支
/// 触发 `20113 BIZ_CUSTOMER_IN_USE`。绕开 part 域 CRUD（part CRUD 不是本任务范畴）。
async fn insert_part_with_customer(pool: &PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
        1_577_836_800_000,
        1,
    );
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, version, created_at, updated_at) \
         VALUES ($1, 'TEST-NAME', 'TEST-DWG', 'TEST-APPLICANT', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $2, 0, $3, $3)",
        id,
        customer_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part referencing customer");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_customer_root_then_l2_then_soft_delete_in_use() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "cust_admin").await;

    // Create L1
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/customers",
            Some(json!({"name": "ACME", "serial_prefix": "A"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "create L1: {env1}");
    assert_eq!(env1["code"], 0);
    let l1_id = env1["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(env1["data"]["serial_prefix"], "A");

    // Create L2 (parent_id = l1_id)
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            "/customers",
            Some(json!({
                "name": "ACME-Workshop1",
                "parent_id": l1_id.clone(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED, "create L2: {env2}");
    assert_eq!(env2["code"], 0);
    assert_eq!(env2["data"]["parent_id"], l1_id);

    // Insert a t_part referencing L1 directly so soft-delete check fires
    // (brief's service logic only inspects t_part / t_assembly).
    insert_part_with_customer(&pool, l1_id.parse::<i64>().unwrap()).await;

    // Soft-delete L1 → should fail with 20113 BIZ_CUSTOMER_IN_USE
    let (s3, env3) = send(
        app,
        json_request(
            "POST",
            &format!("/customers/{l1_id}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s3,
        StatusCode::CONFLICT,
        "soft-delete should return 409 CONFLICT for BIZ_CUSTOMER_IN_USE; got {env3}"
    );
    assert_eq!(
        env3["code"].as_i64().unwrap(),
        20113,
        "expected BIZ_CUSTOMER_IN_USE; got envelope: {env3}"
    );
}

#[tokio::test]
async fn update_customer_serial_prefix_collision_returns_20104() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "cust_prefix").await;

    // Create two L1 customers with distinct serial_prefix values.
    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/customers",
            Some(json!({"name": "Alpha", "serial_prefix": "A"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    let l1_a_id = env1["data"]["id"].as_str().unwrap().to_string();

    let (_s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            "/customers",
            Some(json!({"name": "Bravo", "serial_prefix": "B"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env2["code"], 0);

    // Try to rename A → "B". uq_t_customer_root_prefix fires → 23505 → 20104.
    let (s3, env3) = send(
        app,
        json_request(
            "POST",
            &format!("/customers/{l1_a_id}/update"),
            Some(json!({"serial_prefix": "B"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s3,
        StatusCode::BAD_REQUEST,
        "duplicate-prefix update should return 400 BIZ_INVALID_VALUE; got {env3}"
    );
    assert_eq!(
        env3["code"].as_i64().unwrap(),
        20104,
        "expected BIZ_INVALID_VALUE (20104); got envelope: {env3}"
    );
    assert!(
        env3["message"]
            .as_str()
            .unwrap_or_default()
            .contains("serial_prefix 已存在"),
        "expected message to contain 'serial_prefix 已存在'; got: {env3}"
    );
}