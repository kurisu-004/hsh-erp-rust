//! part 域集成测试 —— batch-pass-inspection + 单件 pass-inspection
//!
//! 覆盖（设计 §6 / §10）：
//!   1. 批量送检 happy path —— 3 个 INSPECTION 工单 → 200 / passed=3 / failed=0
//!   2. 批量送检 partial failure —— 2 INSPECTION + 1 IN_PROCESS → 200 / passed=2 / failed=1 (20103)
//!   3. 批量送检空 items 校验 —— 422 / 40001 VALIDATION_ERROR
//!   4. 批量送检 CLERK 越权 —— 403 / 40300 FORBIDDEN
//!   5. 单件送检 happy path —— POST /{part_id}/pass-inspection → 200 / status=READY_TO_SHIP
//!   6. 单件送检 OCC retry —— 第二次同件送检 → 400 / 20103 BIZ_INVALID_TRANSITION
//!      （READY_TO_SHIP → READY_TO_SHIP 被状态机白名单拒绝，不是 40901 VERSION_CONFLICT）
//!
//! ## 并行 / 认证
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。
//! 每个用例 MANAGER 或 INSPECTOR token。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{add_role, insert_user_with_password, test_app, test_state};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

// ===========================================================================
//  全局串行化 + helpers（拷贝自 tests/delivery_scan_api.rs，按约定不跨文件复用）
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
    let state = test_state(pool.clone());
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

async fn login_inspector(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "INSPECTOR", None, None).await;
    let state = test_state(pool.clone());
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

async fn login_clerk(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "CLERK", None, None).await;
    let state = test_state(pool.clone());
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

// ----- Fixture helpers（拷贝自 tests/delivery_scan_api.rs，按约定不跨文件复用） -----

async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id, name, prefix, now,
    )
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id, name, l1_id, now,
    )
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

/// 带 status 参数的 part 插入（适配 INSPECTION / IN_PROCESS / READY_TO_SHIP 等）。
///
/// 与 `tests/delivery_scan_api.rs::insert_part` 不同：原 fixture 硬编码 `status="INSPECTION"`，
/// 本测试需要构造 partial-failure（混合 INSPECTION / IN_PROCESS），故参数化 status。
async fn insert_part_with_status(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    assembly_id: Option<i64>,
    status: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, \
         quantity, has_been_repaired, version, created_at, created_by, updated_at, updated_by, \
         assembly_id) \
         VALUES ($1, $2, $3, 'D-001', $4, $8, $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL, $7)",
        id, serial_no, name, customer_id, now, today, assembly_id, status,
    )
    .execute(pool)
    .await
    .expect("insert part");
    id
}

async fn insert_batch(pool: &PgPool, part_id: i64, batch_no: i32, qty: i32, status: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, has_been_repaired, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, false, 0, $6, NULL, $6, NULL)",
        id, part_id, batch_no, qty, status, now,
    )
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 批量送检 happy path：3 个 INSPECTION 工单 → 200 / passed=3 / failed=0。
#[tokio::test]
async fn batch_pass_inspection_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let mut pids = Vec::new();
    for i in 0..3 {
        let pid = insert_part_with_status(
            &pool,
            &format!("P{i}"),
            l2,
            Some(&format!("P{i:03}")),
            None,
            "INSPECTION",
        )
        .await;
        let _ = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
        pids.push(pid);
    }

    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "items": pids
            .iter()
            .map(|p| json!({"part_id": p.to_string()}))
            .collect::<Vec<_>>()
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-pass-inspection",
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "happy path: {env}");
    assert_eq!(env["code"], 0);
    let passed = env["data"]["passed"].as_array().expect("data.passed");
    let failed = env["data"]["failed"].as_array().expect("data.failed");
    assert_eq!(passed.len(), 3, "应 passed=3: {env}");
    assert_eq!(failed.len(), 0, "应 failed=0: {env}");
    // 全部都是 READY_TO_SHIP
    for p in passed {
        assert_eq!(p["status"], "READY_TO_SHIP");
    }
}

/// 批量送检 partial failure：3 个工单（2 INSPECTION + 1 IN_PROCESS）→ 200 /
/// passed=2 / failed=1 (code=20103)。
#[tokio::test]
async fn batch_pass_inspection_partial_failure() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let mut items = Vec::new();
    for i in 0..3 {
        let status = if i == 1 { "IN_PROCESS" } else { "INSPECTION" };
        let pid = insert_part_with_status(
            &pool,
            &format!("P{i}"),
            l2,
            Some(&format!("P{i:03}")),
            None,
            status,
        )
        .await;
        let _ = insert_batch(&pool, pid, 1, 1, status).await;
        items.push(json!({"part_id": pid.to_string()}));
    }

    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-pass-inspection",
            Some(json!({ "items": items })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "partial failure: {env}");
    assert_eq!(env["code"], 0);
    let passed = env["data"]["passed"].as_array().expect("data.passed");
    let failed = env["data"]["failed"].as_array().expect("data.failed");
    assert_eq!(passed.len(), 2, "应 passed=2: {env}");
    assert_eq!(failed.len(), 1, "应 failed=1: {env}");
    assert_eq!(failed[0]["code"], 20103, "失败码应为 20103: {env}");
    // message 来自 AppError Display: "[20103] part <id> 当前状态 IN_PROCESS 不允许品检通过"
    let msg = failed[0]["message"].as_str().expect("failed.message");
    assert!(msg.starts_with("[20103]"), "message 应以 [20103] 开头: {msg}");
    assert!(msg.contains("IN_PROCESS"), "message 应含 IN_PROCESS: {msg}");
    // part_id 应当是 IN_PROCESS 的 P001（i=1）
    let failed_pid = failed[0]["part_id"].as_str().expect("failed.part_id is string");
    let expected = items[1]["part_id"].as_str().unwrap().to_string();
    assert_eq!(failed_pid, expected);
}

/// 批量送检 items=[] → 422 / 40001 VALIDATION_ERROR（handler 兜底校验）。
#[tokio::test]
async fn batch_pass_inspection_empty_items_40001() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-pass-inspection",
            Some(json!({"items": []})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "empty items: {env}");
    assert_eq!(env["code"], 40001);
}

/// 批量送检 CLERK 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn batch_pass_inspection_clerk_forbidden() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_clerk(pool, "clerk1").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-pass-inspection",
            Some(json!({"items": [{"part_id": "1"}]})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "clerk forbidden: {env}");
    assert_eq!(env["code"], 40300);
}

/// 单件送检 happy path：1 个 INSPECTION 工单 → 200 / status=READY_TO_SHIP。
///
/// 使用 INSPECTOR 角色验证「Manager 或 Inspector」白名单。
#[tokio::test]
async fn single_pass_inspection_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "INSPECTION",
    )
    .await;
    let _ = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;

    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/pass-inspection"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "single happy: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "READY_TO_SHIP");
    assert_eq!(
        env["data"]["id"].as_str().unwrap().to_string(),
        pid.to_string()
    );
}

/// 单件送检 OCC retry：第二次送检 → 400 / 20103 BIZ_INVALID_TRANSITION
/// （READY_TO_SHIP → READY_TO_SHIP 被状态机白名单拒绝，不是 40901 VERSION_CONFLICT）。
///
/// 任务 3 review note：第二次送检走状态机守卫，状态机白名单不含
/// `READY_TO_SHIP → READY_TO_SHIP`，返回 20103 而非 40901。
#[tokio::test]
async fn single_pass_inspection_retry_returns_20103() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "INSPECTION",
    )
    .await;
    let _ = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;

    let (app, token, _pool) = login_manager(pool, "admin").await;

    // 1st call：happy path
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{pid}/pass-inspection"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first call: {env1}");
    assert_eq!(env1["data"]["status"], "READY_TO_SHIP");

    // 2nd call：状态机拒绝，20103 BIZ_INVALID_TRANSITION
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/pass-inspection"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::BAD_REQUEST,
        "second call should be 400 (state machine reject): {env2}"
    );
    assert_eq!(
        env2["code"], 20103,
        "second call should be 20103 BIZ_INVALID_TRANSITION (READY_TO_SHIP → READY_TO_SHIP), got: {env2}"
    );
}