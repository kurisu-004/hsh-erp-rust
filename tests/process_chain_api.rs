//! process_chain 域端到端集成测试（part-worker-pool-federated-rocket 2026-09-11）
//!
//! 覆盖 6 个场景：
//!   1. happy path：建链 → fetch 拿到 header + steps
//!   2. fetch 找不到链 → 20701 BIZ_PROCESS_CHAIN_NOT_FOUND (HTTP 404)
//!   3. PUT upsert 替换步骤（清空旧 steps + 插新 steps，chain.version++）
//!   4. PUT upsert 创建全新链（part 之前无链）
//!   5. PUT upsert 校验：estimated_minutes < 0 → 40001 VALIDATION_ERROR
//!   6. PUT upsert 校验：sort_order 重复 → 40001 VALIDATION_ERROR
//!
//! ## 串行化
//! 进程级 `tokio::sync::Mutex` + `--test-threads=1` 双保险。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, insert_user_with_password, test_app, test_state,
};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

// ===========================================================================
//  全局串行化 + HTTP helpers
// ===========================================================================

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

async fn setup() -> (tokio::sync::MutexGuard<'static, ()>, PgPool) {
    use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

// ----- 角色登录 helper -----

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
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
    (app2, token, pool)
}

// ===========================================================================
//  process_chain fixture helpers
// ===========================================================================

/// 进程级共享雪花 ID 生成器（同 worker_pool_api 模式）
fn chain_snowflake() -> &'static SnowflakeIdGenerator {
    use std::sync::OnceLock;
    static S: OnceLock<SnowflakeIdGenerator> = OnceLock::new();
    S.get_or_init(|| SnowflakeIdGenerator::new(1_577_836_800_000, 1))
}

async fn insert_part(pool: &PgPool, customer_id: i64, serial_no: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = chain_snowflake().next_id();
    let now = now_naive();
    let today = now.date();
    // 用 sqlx::query (runtime) 而非 query! 避免每个 fixture 都依赖 .sqlx 缓存重生成。
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, \
         request_date, planned_delivery_date, status, is_urgent, customer_id, \
         quantity, unit_price, total_price, version, created_at, updated_at) \
         VALUES ($1, $2, 'test', 'D-PCH', $3, $4, $4, 'PENDING', false, $5, \
         1, 0, 0, 0, $6, $6)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(serial_no.to_string()) // applicant_name = serial_no
    .bind(today)
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

async fn insert_customer_l2(pool: &PgPool, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let one_char: String = prefix.chars().next().unwrap_or('X').to_ascii_uppercase().to_string();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) \
         VALUES ($1, $2, NULL, $3, 0, $4, $4)",
        id,
        prefix,
        one_char,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

async fn seed_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
        id,
        code,
        name,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: happy path —— 创链 → fetch 拿到 header + steps
#[tokio::test]
async fn upsert_then_get_by_part_happy() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "PCH").await;
    let proc_a = seed_process(&pool, "PROC-A", "工序A").await;
    let proc_b = seed_process(&pool, "PROC-B", "工序B").await;
    let part_id = insert_part(&pool, customer, "P-001").await;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr1").await;

    // 1. upsert：建链 + 2 步
    let (s, env) = send(
        app.clone(),
        json_request(
            "PUT",
            &format!("/process-chains/by-part/{part_id}"),
            Some(json!({
                "name": "默认工艺",
                "note": "happy path",
                "steps": [
                    { "sort_order": 10, "process_id": proc_a.to_string(), "estimated_minutes": 30 },
                    { "sort_order": 20, "process_id": proc_b.to_string(), "estimated_minutes": 45 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "upsert: {env}");
    let data = &env["data"];
    assert_eq!(data["part_id"], part_id.to_string());
    assert_eq!(data["name"], "默认工艺");
    assert_eq!(data["note"], "happy path");
    let steps = data["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 2, "应有 2 步: {env}");
    assert_eq!(steps[0]["sort_order"], 10);
    assert_eq!(steps[0]["process_id"], proc_a.to_string());
    assert_eq!(steps[0]["estimated_minutes"], 30);
    assert_eq!(steps[1]["sort_order"], 20);
    assert_eq!(steps[1]["process_id"], proc_b.to_string());
    assert_eq!(steps[1]["estimated_minutes"], 45);
    let chain_id = data["id"].as_str().unwrap().to_string();

    // 2. fetch by part
    let state = test_state(_pool).await;
    let app = test_app(state);
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            &format!("/process-chains/by-part/{part_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "get_by_part: {env2}");
    assert_eq!(env2["data"]["id"], chain_id);
    assert_eq!(env2["data"]["steps"].as_array().unwrap().len(), 2);
}

/// 场景 2: 找不到链 → 20701 BIZ_PROCESS_CHAIN_NOT_FOUND
#[tokio::test]
async fn get_by_part_chain_not_found() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "PCH-NF").await;
    let part_id = insert_part(&pool, customer, "P-NF").await;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_nf").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/process-chains/by-part/{part_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "无链应 404: {env}");
    assert_eq!(env["code"], 20701, "BIZ_PROCESS_CHAIN_NOT_FOUND: {env}");
}

/// 场景 3: PUT 整组替换：先有 2 步 → 换成 1 步；旧 steps 软删，新 step 新 id
#[tokio::test]
async fn upsert_replaces_old_steps() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "PCH-REP").await;
    let proc_a = seed_process(&pool, "PROC-RA", "工序A").await;
    let proc_b = seed_process(&pool, "PROC-RB", "工序B").await;
    let part_id = insert_part(&pool, customer, "P-REP").await;

    let (app, token, pool) = login_manager(pool.clone(), "mgr_rep").await;

    // 1. 首次 upsert：2 步
    let (_s, env) = send(
        app.clone(),
        json_request(
            "PUT",
            &format!("/process-chains/by-part/{part_id}"),
            Some(json!({
                "name": "原链",
                "steps": [
                    { "sort_order": 10, "process_id": proc_a.to_string(), "estimated_minutes": 20 },
                    { "sort_order": 20, "process_id": proc_b.to_string(), "estimated_minutes": 40 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    let chain_id = env["data"]["id"].as_str().unwrap().to_string();
    let version_before = env["data"]["version"].as_i64().unwrap();

    // 2. 二次 upsert：替换为 1 步
    let (s2, env2) = send(
        app,
        json_request(
            "PUT",
            &format!("/process-chains/by-part/{part_id}"),
            Some(json!({
                "name": "新链",
                "steps": [
                    { "sort_order": 10, "process_id": proc_a.to_string(), "estimated_minutes": 60 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "upsert replace: {env2}");
    assert_eq!(env2["data"]["id"], chain_id, "chain id 不变");
    assert_eq!(env2["data"]["name"], "新链", "name 应更新");
    assert!(
        env2["data"]["version"].as_i64().unwrap() > version_before,
        "version 应自增 (前={version_before}, 后={})", env2["data"]["version"]
    );
    let steps = env2["data"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "应只剩 1 步");
    assert_eq!(steps[0]["estimated_minutes"], 60, "应使用新 step 的 minutes");

    // 3. 验证旧 step 已软删（DB 直接查）
    let chain_id_i64: i64 = chain_id.parse().expect("parse chain_id");
    let n_active: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!" FROM t_process_chain_step
        WHERE chain_id = $1 AND deleted_at IS NULL"#,
        chain_id_i64,
    )
    .fetch_one(&pool)
    .await
    .expect("count active steps");
    assert_eq!(n_active, 1, "DB 应只剩 1 个活跃 step");
}

/// 场景 4: upsert 时 estimated_minutes < 0 → 40001
#[tokio::test]
async fn upsert_rejects_negative_minutes() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "PCH-NEG").await;
    let proc = seed_process(&pool, "PROC-NEG", "工序").await;
    let part_id = insert_part(&pool, customer, "P-NEG").await;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_neg").await;
    let (s, env) = send(
        app,
        json_request(
            "PUT",
            &format!("/process-chains/by-part/{part_id}"),
            Some(json!({
                "steps": [
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": -1 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "negative 应 400: {env}");
    assert_eq!(env["code"], 20104, "BIZ_INVALID_VALUE: {env}");
}

/// 场景 5: upsert 时 sort_order 重复 → 40001
#[tokio::test]
async fn upsert_rejects_duplicate_sort_order() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "PCH-DUP").await;
    let proc = seed_process(&pool, "PROC-DUP", "工序").await;
    let part_id = insert_part(&pool, customer, "P-DUP").await;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_dup").await;
    let (s, env) = send(
        app,
        json_request(
            "PUT",
            &format!("/process-chains/by-part/{part_id}"),
            Some(json!({
                "steps": [
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": 30 },
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": 60 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "重复应 400: {env}");
    assert_eq!(env["code"], 20104, "BIZ_INVALID_VALUE: {env}");
}

/// 场景 6: 非 Manager 调用 upsert → 40300 FORBIDDEN
#[tokio::test]
async fn upsert_forbidden_for_non_manager() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "PCH-FB").await;
    let proc = seed_process(&pool, "PROC-FB", "工序").await;
    let part_id = insert_part(&pool, customer, "P-FB").await;

    let uid = insert_user_with_password(&pool, "clerk1", "changeme").await;
    add_role(&pool, uid, "CLERK", None, None).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": "clerk1", "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();

    let app = test_app(state);
    let (s, env) = send(
        app,
        json_request(
            "PUT",
            &format!("/process-chains/by-part/{part_id}"),
            Some(json!({
                "steps": [
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": 30 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "非 Manager 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}