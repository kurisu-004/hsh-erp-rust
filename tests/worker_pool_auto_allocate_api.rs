//! worker_pool::auto_allocate 端到端集成测试（part-worker-pool-federated-rocket 2026-09-11）
//!
//! 覆盖 7 个场景：
//!   1. COUNT mode happy：fill_ratio=1.0 抢到 max_held_batches 个批次
//!   2. COUNT mode fill_ratio=0.0 抢到 0 批
//!   3. TIME mode happy：max_held_minutes=120、fill_ratio=0.5 → target=60 分钟
//!   4. TIME mode 但 max_held_minutes IS NULL → 20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET
//!   5. fill_ratio ∉ [0.0, 1.0] → 20704 BIZ_AUTO_ALLOCATE_INVALID_RATIO
//!   6. 池空 → pool_empty=true，filled.len() = 0
//!   7. process_id 不存在 → 20801 BIZ_PROCESS_NOT_FOUND
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
    add_role, insert_user_with_password, link_shelf_to_process, link_work_type_to_process,
    seed_process, test_app, test_state,
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
//  worker-pool auto_allocate fixture helpers
// ===========================================================================

fn pool_snowflake() -> &'static SnowflakeIdGenerator {
    use std::sync::OnceLock;
    static S: OnceLock<SnowflakeIdGenerator> = OnceLock::new();
    S.get_or_init(|| SnowflakeIdGenerator::new(1_577_836_800_000, 1))
}

async fn insert_work_type(
    pool: &PgPool,
    code: &str,
    name: &str,
    max_held_batches: Option<i32>,
    max_held_minutes: Option<i32>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_work_type (id, code, name, sort_order, version, \
         max_held_batches, max_held_minutes, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $5, $6, $6)",
        id,
        code,
        name,
        max_held_batches,
        max_held_minutes,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_work_type");
    id
}

async fn insert_worker(
    pool: &PgPool,
    badge_code: &str,
    name: &str,
    work_type_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, true, $4, 0, $5, $5)",
        id,
        badge_code,
        name,
        work_type_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_worker");
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

async fn insert_pool_part(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
    shelf_id: i64,
    process_id: i64,
    quantity: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let now = now_naive();
    let today = now.date();
    let part_id = pool_snowflake().next_id();
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, \
         request_date, planned_delivery_date, system_delivery_date, status, location, \
         is_urgent, current_holder_id, placed_at, next_process_id, customer_id, \
         quantity, version, created_at, updated_at) \
         VALUES ($1, $2, 'pool-item', 'D-POOL', $2, $4, $4, $4, 'IN_PROCESS', \
         'PRODUCTION_SHELF', false, $5, $3, $6, $7, $8, 0, $3, $3)",
        part_id,
        serial_no,
        now,
        today,
        shelf_id,
        process_id,
        customer_id,
        quantity,
    )
    .execute(pool)
    .await
    .expect("insert t_part");
    let batch_id = pool_snowflake().next_id();
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, next_process_id, placed_at, has_been_repaired, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, 'IN_PROCESS', 'PRODUCTION_SHELF', $4, $5, $6, false, 0, $6, $6)",
        batch_id,
        part_id,
        quantity,
        shelf_id,
        process_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    (part_id, batch_id)
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: COUNT mode happy —— fill_ratio=1.0 抢满 max_held_batches
#[tokio::test]
async fn auto_allocate_count_mode_full_fill() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "AC1").await;
    let proc = seed_process(&pool, "PROC-AC1", "工序").await;
    let wt = insert_work_type(&pool, "WT-AC1", "工种", Some(5), None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = common::insert_shelf(&pool, "PROD-AC1", "PROD-AC1", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let worker = insert_worker(&pool, "BC-AC1", "工", Some(wt)).await;
    for i in 0..10 {
        let sn = format!("AC1-P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, shelf, proc, 1).await;
    }

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_ac1").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
                "mode": "COUNT",
                "fill_ratio": 1.0,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto_allocate: {env}");
    let data = &env["data"];
    assert_eq!(data["process_id"], proc.to_string());
    assert_eq!(data["mode"], "COUNT");
    assert_eq!(data["fill_ratio"], 1.0);
    let filled = data["filled"].as_array().expect("filled array");
    assert_eq!(filled.len(), 1, "应只 1 个 worker: {env}");
    assert_eq!(filled[0]["worker_id"], worker.to_string());
    assert_eq!(filled[0]["target"], 5, "max=5, ratio=1.0 → target=5");
    assert_eq!(filled[0]["filled_count"], 5, "池里有 10 个 → 应抢 5 个");
    assert_eq!(data["pool_empty"], false);
}

/// 场景 2: COUNT mode fill_ratio=0 → 抢 0 个
#[tokio::test]
async fn auto_allocate_count_mode_zero_fill() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "AC2").await;
    let proc = seed_process(&pool, "PROC-AC2", "工序").await;
    let wt = insert_work_type(&pool, "WT-AC2", "工种", Some(10), None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = common::insert_shelf(&pool, "PROD-AC2", "PROD-AC2", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let _worker = insert_worker(&pool, "BC-AC2", "工", Some(wt)).await;
    for i in 0..5 {
        let sn = format!("AC2-P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, shelf, proc, 1).await;
    }

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_ac2").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
                "mode": "COUNT",
                "fill_ratio": 0.0,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto_allocate zero: {env}");
    let filled = env["data"]["filled"].as_array().unwrap();
    assert_eq!(filled[0]["target"], 0);
    assert_eq!(filled[0]["filled_count"], 0);
    assert_eq!(env["data"]["pool_empty"], false);
}

/// 场景 3: TIME mode target 计算正确 —— max_held_minutes=120、fill_ratio=0.5 → target=60
///
/// 注：本端点 TIME 模式的"target=累计分钟数"语义当前是按 `target = ceil(max × ratio)`
/// 取 max_held_minutes 整数倍；本测试只验证 target 计算正确 + 池空时 break。
/// （`take_one_from_pool` 的 CTE 内 SQL guard 需要 `max_held_batches` 才能成功取数，
/// 但本测试场景下池里只放 1 件，pool_empty=true 即可；target 数值仍按 60 验证。）
#[tokio::test]
async fn auto_allocate_time_mode_target_calc() {
    let (_guard, pool) = setup().await;
    let proc = seed_process(&pool, "PROC-AC3", "工序").await;
    // TIME 模式需要 max_held_minutes 设置；同时为兼容 take_one_from_pool CTE 也设 max_held_batches
    let wt = insert_work_type(&pool, "WT-AC3", "工种", Some(5), Some(120)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = common::insert_shelf(&pool, "PROD-AC3", "PROD-AC3", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let _worker = insert_worker(&pool, "BC-AC3", "工", Some(wt)).await;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_ac3").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
                "mode": "TIME",
                "fill_ratio": 0.5,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto_allocate TIME: {env}");
    let filled = env["data"]["filled"].as_array().unwrap();
    assert_eq!(filled[0]["target"], 60, "max=120, ratio=0.5 → target=60");
    // 池里没件 → 抢 0 个 + pool_empty=true
    assert_eq!(filled[0]["filled_count"], 0, "池里没件 → 抢 0 个");
    assert_eq!(env["data"]["pool_empty"], true, "循环中池空");
}

/// 场景 4: TIME mode 但 max_held_minutes IS NULL → 20703
#[tokio::test]
async fn auto_allocate_time_mode_minutes_not_set() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "AC4").await;
    let proc = seed_process(&pool, "PROC-AC4", "工序").await;
    // max_held_batches 也设，max_held_minutes NULL
    let wt = insert_work_type(&pool, "WT-AC4", "工种", Some(5), None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = common::insert_shelf(&pool, "PROD-AC4", "PROD-AC4", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let _worker = insert_worker(&pool, "BC-AC4", "工", Some(wt)).await;
    insert_pool_part(&pool, customer, "AC4-P-001", shelf, proc, 1).await;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_ac4").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
                "mode": "TIME",
                "fill_ratio": 0.5,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "max_held_minutes NULL 应 400: {env}");
    assert_eq!(env["code"], 20703, "BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET: {env}");
}

/// 场景 5: fill_ratio > 1.0 → 20704
#[tokio::test]
async fn auto_allocate_rejects_ratio_above_one() {
    let (_guard, pool) = setup().await;
    let proc = seed_process(&pool, "PROC-AC5", "工序").await;
    let shelf = common::insert_shelf(&pool, "PROD-AC5", "PROD-AC5", "PRODUCTION").await;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_ac5").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
                "mode": "COUNT",
                "fill_ratio": 1.5,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "ratio > 1.0 应 400: {env}");
    assert_eq!(env["code"], 20704, "BIZ_AUTO_ALLOCATE_INVALID_RATIO: {env}");
}

/// 场景 5b: fill_ratio < 0.0 → 20704
#[tokio::test]
async fn auto_allocate_rejects_negative_ratio() {
    let (_guard, pool) = setup().await;
    let proc = seed_process(&pool, "PROC-AC5B", "工序").await;
    let shelf = common::insert_shelf(&pool, "PROD-AC5B", "PROD-AC5B", "PRODUCTION").await;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_ac5b").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
                "mode": "COUNT",
                "fill_ratio": -0.1,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "ratio < 0 应 400: {env}");
    assert_eq!(env["code"], 20704, "BIZ_AUTO_ALLOCATE_INVALID_RATIO: {env}");
}

/// 场景 6: 池空 → pool_empty=true
#[tokio::test]
async fn auto_allocate_pool_empty() {
    let (_guard, pool) = setup().await;
    let proc = seed_process(&pool, "PROC-AC6", "工序").await;
    let wt = insert_work_type(&pool, "WT-AC6", "工种", Some(5), None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = common::insert_shelf(&pool, "PROD-AC6", "PROD-AC6", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let _worker = insert_worker(&pool, "BC-AC6", "工", Some(wt)).await;
    // 不插任何 pool_part

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_ac6").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
                "mode": "COUNT",
                "fill_ratio": 1.0,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "empty pool 应 200: {env}");
    let filled = env["data"]["filled"].as_array().unwrap();
    assert_eq!(filled[0]["filled_count"], 0, "池空 → 0 件");
    assert_eq!(env["data"]["pool_empty"], true);
}

/// 场景 7: process_id 不存在 → 20801
#[tokio::test]
async fn auto_allocate_process_not_found() {
    let (_guard, pool) = setup().await;
    let shelf = common::insert_shelf(&pool, "PROD-AC7", "PROD-AC7", "PRODUCTION").await;
    let nonexistent: i64 = 9_999_999_999_999;

    let (app, token, _pool) = login_manager(pool.clone(), "mgr_ac7").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/auto-allocate",
            Some(json!({
                "process_id": nonexistent.to_string(),
                "shelf_id": shelf.to_string(),
                "mode": "COUNT",
                "fill_ratio": 0.5,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "无 process 应 404: {env}");
    assert_eq!(env["code"], 20801, "BIZ_PROCESS_NOT_FOUND: {env}");
}