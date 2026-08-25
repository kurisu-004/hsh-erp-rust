//! worker_pool 域端到端集成测试（Task 10 / plan §11）
//!
//! 覆盖 16 个场景：
//!   1. worker_scan INSPECTED → 自动 refill
//!   2. worker_scan RETURNED → 自动 refill
//!   3. refill_when_pool_empty_returns_empty
//!   4. refill_caps_at_max_held_batches
//!   5. concurrent_refill_no_double_pick       [`#[ignore]`：需 app-level 并发基建]
//!   6. refill_respects_shelf_scope            （同 7 节外）
//!   7. refill_skips_concurrently_modified_batch [`#[ignore]`：需并发模拟基建]
//!   8. worker_scan_shelf_scope_violation_403
//!   9. take_updates_t_part_holder
//!  10. take_does_not_update_placed_at
//!  11. events_persisted_to_t_part_event
//!  12. admin_refill_endpoint_works
//!  13. admin_remove_returns_batch_to_pool
//!  14. refill_failure_rolls_back_worker_scan   [`#[ignore]`：DB 故障注入缺基建]
//!  15. max_held_null_returns_error
//!  16. worker_no_work_type_returns_error
//!
//! ## 串行化
//! 进程级 `tokio::sync::Mutex` + `--test-threads=1` 双保险。共享 `postgres_rust_test` 库。

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

/// SHELF_ACCOUNT user：scope 限制在指定 shelves（不传 → wildcard 全开放）。
async fn login_shelf_account(
    pool: PgPool,
    username: &str,
    shelves: &[i64],
) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    for sid in shelves {
        add_role(&pool, uid, "SHELF_ACCOUNT", Some("shelf"), Some(*sid)).await;
    }
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

// ===========================================================================
//  worker-pool fixture helpers
// ===========================================================================

async fn insert_work_type(pool: &PgPool, code: &str, name: &str, max_held: Option<i32>) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_work_type (id, code, name, sort_order, version, \
         max_held_batches, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $5, $5)",
        id,
        code,
        name,
        max_held,
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let l1_id = snowflake.next_id();
    let now = now_naive();
    // serial_prefix is varchar(1) + regex ^[A-Z]$ — pick first char uppercased
    let one_char: String = prefix.chars().next().unwrap_or('X').to_ascii_uppercase().to_string();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) \
         VALUES ($1, $2, NULL, $3, 0, $4, $4)",
        l1_id,
        prefix,
        one_char,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1");
    l1_id
}

/// 插一个 IN_PROCESS+PRODUCTION_SHELF 工单 + 批次 + placed_at。
/// 返回 (part_id, batch_id)。
async fn insert_pool_part(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
    shelf_id: i64,
    process_id: i64,
    quantity: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let now = now_naive();
    let today = now.date();
    let part_id = snowflake.next_id();
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
    let batch_id = snowflake.next_id();
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

/// worker-scan happy path 准备：worker 当前持有 1 个 IN_PROCESS+WORKER 批次。
///
/// 入参：
/// - `worker_id`：worker.id
/// - `shelf_id`：RETURNED 目标 shelf
/// - `process_id`：batch.next_process_id（必填 RETURNED）
///
/// 返回 (part_id, batch_id)。
async fn insert_worker_held_part(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
    worker_id: i64,
    next_process_id: i64,
    quantity: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let now = now_naive();
    let today = now.date();
    let part_id = snowflake.next_id();
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, \
         request_date, planned_delivery_date, system_delivery_date, status, location, \
         is_urgent, current_holder_id, placed_at, next_process_id, customer_id, \
         quantity, version, created_at, updated_at) \
         VALUES ($1, $2, 'held-item', 'D-HELD', $2, $4, $4, $4, 'IN_PROCESS', \
         'WORKER', false, $5, $3, $6, $7, $8, 0, $3, $3)",
        part_id,
        serial_no,
        now,
        today,
        worker_id,
        next_process_id,
        customer_id,
        quantity,
    )
    .execute(pool)
    .await
    .expect("insert held t_part");
    let batch_id = snowflake.next_id();
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, next_process_id, placed_at, has_been_repaired, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, 'IN_PROCESS', 'WORKER', $4, $5, $6, false, 0, $6, $6)",
        batch_id,
        part_id,
        quantity,
        worker_id,
        next_process_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert held t_part_batch");
    (part_id, batch_id)
}

/// 把 part_id 给定批次标为 worker 持有（针对 pool→worker 流转后的批次）。
async fn count_held_by_worker(pool: &PgPool, worker_id: i64) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!"
        FROM t_part_batch
        WHERE current_holder_id = $1 AND location = 'WORKER' AND deleted_at IS NULL"#,
        worker_id,
    )
    .fetch_one(pool)
    .await
    .expect("count held")
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: worker-scan INSPECTED → 自动 refill
#[tokio::test]
async fn worker_scan_inspected_triggers_refill() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL").await;
    let proc = seed_process(&pool, "PROC-A", "工序A").await;
    let wt = insert_work_type(&pool, "WT-A", "工种A", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-A", "PROD-A", "PRODUCTION").await;
    let insp_shelf = common::insert_shelf(&pool, "INSP-A", "INSP-A", "INSPECTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC001", "工1", Some(wt)).await;
    // worker 当前持 1 件；池里 1 件待 refill
    let (held_part, _held_batch) =
        insert_worker_held_part(&pool, customer, "H-001", worker, proc, 1).await;
    let (_pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-001", prod_shelf, proc, 1).await;

    let (app, token, pool) =
        login_shelf_account(pool.clone(), "user1", &[prod_shelf, insp_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/worker-scan",
            Some(json!({
                "serial_no": "H-001",
                "badge_code": "BC001",
                "event_type": "INSPECTED",
                "shelf_id": prod_shelf.to_string(),
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan INSPECTED: {env}");
    assert_eq!(env["code"], 0);
    // scan 出参：worker_id / part_id / event_type 都在 scan 字段里
    assert_eq!(env["data"]["scan"]["worker_id"], worker.to_string());
    assert_eq!(env["data"]["scan"]["part_id"], held_part.to_string());
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_INSPECTED");
    // refill 出参：从池里抢到 1 件 + pool_empty=false
    let taken = env["data"]["refill"]["taken"].as_array().expect("refill.taken");
    assert_eq!(taken.len(), 1, "refill 应抢到 1 件: {env}");
    assert_eq!(env["data"]["refill"]["pool_empty"], false);

    // 验证 worker 持有数 = 1（放回 1 + 抢到 1）
    let held = count_held_by_worker(&pool, worker).await;
    assert_eq!(held, 1, "worker 应持有 1 件（refill 后）");
}

/// 场景 2: worker-scan RETURNED → 自动 refill
#[tokio::test]
async fn worker_scan_returned_triggers_refill() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL2").await;
    let proc = seed_process(&pool, "PROC-B", "工序B").await;
    let wt = insert_work_type(&pool, "WT-B", "工种B", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-B", "PROD-B", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC002", "工2", Some(wt)).await;
    let (_held_part, _held_batch) =
        insert_worker_held_part(&pool, customer, "H-002", worker, proc, 1).await;
    let (_pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-002", prod_shelf, proc, 1).await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/worker-scan",
            Some(json!({
                "serial_no": "H-002",
                "badge_code": "BC002",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_RETURNED");
    let taken = env["data"]["refill"]["taken"].as_array().expect("refill.taken");
    // REFILL 抢满 max=5；池里放回 1 件（H-002）+ 原 1 件 → refill 抢 2 件
    assert_eq!(taken.len(), 2, "refill 应抢到 2 件（returned 1 + pool 1）: {env}");
}

/// 场景 3: 池空时 refill 返回 empty + pool_empty=true
#[tokio::test]
async fn refill_when_pool_empty_returns_empty() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL3").await;
    let proc = seed_process(&pool, "PROC-C", "工序C").await;
    let wt = insert_work_type(&pool, "WT-C", "工种C", Some(10)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-C", "PROD-C", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC003", "工3", Some(wt)).await;
    // worker 当前不持有任何，池里只有 1 件（max=10，refill 拿走 1 后池空）
    let (_pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-003", prod_shelf, proc, 1).await;

    let (app, token, _pool) = login_manager(pool.clone(), "admin3").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin refill: {env}");
    assert_eq!(env["code"], 0);
    // 池里只 1 件，refill 取走 1 → taken.len()=1, pool_empty=false
    let taken = env["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken.len(), 1, "应 taken=1: {env}");
    assert_eq!(env["data"]["pool_empty"], false);

    // 第二次 refill 池空 → taken=0 + pool_empty=true
    let (app, token, _pool) = login_manager(pool.clone(), "admin3b").await;
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "second refill: {env2}");
    let taken2 = env2["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken2.len(), 0, "应 taken=0: {env2}");
    assert_eq!(env2["data"]["pool_empty"], true);
}

/// 场景 4: refill 上限 = max_held_batches
#[tokio::test]
async fn refill_caps_at_max_held_batches() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL4").await;
    let proc = seed_process(&pool, "PROC-D", "工序D").await;
    let wt = insert_work_type(&pool, "WT-D", "工种D", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-D", "PROD-D", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC004", "工4", Some(wt)).await;
    // 池里塞 20 件，max=5 → refill 应只抢 5
    for i in 0..20 {
        let sn = format!("P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, prod_shelf, proc, 1).await;
    }

    let (app, token, _pool) = login_manager(pool.clone(), "admin4").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin refill: {env}");
    let taken = env["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken.len(), 5, "max=5，应 taken=5: {env}");
    let held = count_held_by_worker(&_pool, worker).await;
    assert_eq!(held, 5, "worker 持有 = 5");
}

/// 场景 5: 并发 refill 不双抢（`#[ignore]`：缺并发 app 基建）
#[tokio::test]
#[ignore = "需要 app-level 并发基建（多 axum server 共享同一 pool）；当前测试用单 Router oneshot，无法验证 SKIP LOCKED 跨事务隔离"]
async fn concurrent_refill_no_double_pick() {
    // 见 README：take_one_from_pool 的 CTE 内含 FOR UPDATE SKIP LOCKED，
    // 单 SQL 原子守卫「held < max」+「row-level skip」，因此两个并发事务
    // 抢同一池时各自拿不同批。本测试需在多 tokio task 中同时调
    // /admin/worker-pool/refill，并断言「两次 taken 总数 = 池大小 + 两次 taken 无交集」。
    // 当前基础设施（单 Router + oneshot）不支持并发，需引入多 Router 共享 state。
}

/// 场景 6: refill 限定 shelf 范围（worker 只能从所绑 shelf 的池里抢）
#[tokio::test]
async fn refill_respects_shelf_scope() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL6").await;
    let proc = seed_process(&pool, "PROC-F", "工序F").await;
    let wt = insert_work_type(&pool, "WT-F", "工种F", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf_a = common::insert_shelf(&pool, "PROD-F1", "PROD-F1", "PRODUCTION").await;
    let shelf_b = common::insert_shelf(&pool, "PROD-F2", "PROD-F2", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_a, proc).await;
    link_shelf_to_process(&pool, shelf_b, proc).await;

    let worker = insert_worker(&pool, "BC006", "工6", Some(wt)).await;
    // shelf_b 上有 2 件，shelf_a 上有 0 件
    for i in 0..2 {
        let sn = format!("PB-{:03}", i);
        insert_pool_part(&pool, customer, &sn, shelf_b, proc, 1).await;
    }
    // refill 时指定 shelf=shelf_a → 池空（shelf_a 上没件）
    let (app, token, _pool) = login_manager(pool.clone(), "admin6").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": shelf_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "refill scope: {env}");
    let taken = env["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken.len(), 0, "shelf_a 上没件，应 taken=0: {env}");
    assert_eq!(env["data"]["pool_empty"], true);
}

/// 场景 7: refill 跳过并发修改的批次（`#[ignore]`：需并发模拟基建）
#[tokio::test]
#[ignore = "需在另一个事务里 UPDATE t_part_batch.version / status 后再触发 refill 并断言 take_one_from_pool 返回 None"]
async fn refill_skips_concurrently_modified_batch() {
    // 设计意图：另一个事务先把候选 batch 的 version+1，refill 的 CTE
    // 用 `pb.version = candidate.version` 守卫，0 行 → 视为池空。
    // 当前 test harness 单 transaction 串行，无法模拟该窗口。
}

/// 场景 8: worker-scan 越权 shelf → 40301 SHELF_MISMATCH
#[tokio::test]
async fn worker_scan_shelf_scope_violation_403() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL8").await;
    let proc = seed_process(&pool, "PROC-H", "工序H").await;
    let wt = insert_work_type(&pool, "WT-H", "工种H", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf_x = common::insert_shelf(&pool, "PROD-H1", "PROD-H1", "PRODUCTION").await;
    let shelf_y = common::insert_shelf(&pool, "PROD-H2", "PROD-H2", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_x, proc).await;
    link_shelf_to_process(&pool, shelf_y, proc).await;

    let worker = insert_worker(&pool, "BC008", "工8", Some(wt)).await;
    let (_held_part, _held_batch) =
        insert_worker_held_part(&pool, customer, "H-008", worker, proc, 1).await;

    // user 只绑定 shelf_x，请求扫描到 shelf_y → 40301 SHELF_MISMATCH
    let (app, token, _pool) = login_shelf_account(pool.clone(), "user8", &[shelf_x]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/worker-scan",
            Some(json!({
                "serial_no": "H-008",
                "badge_code": "BC008",
                "event_type": "RETURNED",
                "shelf_id": shelf_y.to_string(),
                "next_process_id": proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "shelf 越权: {env}");
    assert_eq!(env["code"], 40301, "SHELF_MISMATCH: {env}");
}

/// 场景 9: take 更新 t_part.current_holder_id = worker_id
#[tokio::test]
async fn take_updates_t_part_holder() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL9").await;
    let proc = seed_process(&pool, "PROC-I", "工序I").await;
    let wt = insert_work_type(&pool, "WT-I", "工种I", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-I", "PROD-I", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC009", "工9", Some(wt)).await;
    let (pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-009", prod_shelf, proc, 1).await;

    let (app, token, _pool) = login_manager(pool.clone(), "admin9").await;
    let (_s, _env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;

    // t_part.current_holder_id 应被改为 worker_id
    let holder: Option<i64> = sqlx::query_scalar!(
        "SELECT current_holder_id FROM t_part WHERE id = $1",
        pool_part,
    )
    .fetch_one(&pool)
    .await
    .expect("query holder");
    assert_eq!(holder, Some(worker), "t_part holder 应被更新为 worker.id");
    // t_part.location 应改为 WORKER
    let loc: String = sqlx::query_scalar!(
        "SELECT location AS \"loc!\" FROM t_part WHERE id = $1",
        pool_part,
    )
    .fetch_one(&pool)
    .await
    .expect("query location");
    assert_eq!(loc, "WORKER", "t_part.location 应=WORKER");
}

/// 场景 10: take 不更新 placed_at
#[tokio::test]
async fn take_does_not_update_placed_at() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL10").await;
    let proc = seed_process(&pool, "PROC-J", "工序J").await;
    let wt = insert_work_type(&pool, "WT-J", "工种J", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-J", "PROD-J", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC010", "工10", Some(wt)).await;
    let (pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-010", prod_shelf, proc, 1).await;

    // 记录 take 前的 placed_at
    let placed_before: Option<chrono::NaiveDateTime> = sqlx::query_scalar!(
        "SELECT placed_at FROM t_part WHERE id = $1",
        pool_part,
    )
    .fetch_one(&pool)
    .await
    .expect("query placed_at");
    assert!(placed_before.is_some(), "fixture 应设 placed_at");

    let (app, token, _pool) = login_manager(pool.clone(), "admin10").await;
    let (_s, _env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;

    // placed_at 应保持不变
    let placed_after: Option<chrono::NaiveDateTime> = sqlx::query_scalar!(
        "SELECT placed_at FROM t_part WHERE id = $1",
        pool_part,
    )
    .fetch_one(&pool)
    .await
    .expect("query placed_at");
    assert_eq!(
        placed_before, placed_after,
        "placed_at 不应被 take 修改（fixture vs after）"
    );
}

/// 场景 11: t_part_event 持久化 TAKEN_FROM_POOL / RETURNED_TO_SHELF / SENT_TO_INSPECTION
#[tokio::test]
async fn events_persisted_to_t_part_event() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL11").await;
    let proc = seed_process(&pool, "PROC-K", "工序K").await;
    let wt = insert_work_type(&pool, "WT-K", "工种K", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-K", "PROD-K", "PRODUCTION").await;
    let insp_shelf = common::insert_shelf(&pool, "INSP-K", "INSP-K", "INSPECTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC011", "工11", Some(wt)).await;
    let (held_part, _held_batch) =
        insert_worker_held_part(&pool, customer, "H-011", worker, proc, 1).await;
    let (pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-011", prod_shelf, proc, 1).await;

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "user11", &[prod_shelf, insp_shelf]).await;
    let (_s, _env) = send(
        app,
        json_request(
            "POST",
            "/parts/worker-scan",
            Some(json!({
                "serial_no": "H-011",
                "badge_code": "BC011",
                "event_type": "INSPECTED",
                "shelf_id": prod_shelf.to_string(),
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;

    // 验证两个 part 各有预期事件
    // held_part：SENT_TO_INSPECTION（worker-scan INSPECTED 时写）
    let held_events: Vec<String> = sqlx::query_scalar!(
        r#"SELECT event_type AS "event_type!"
        FROM t_part_event WHERE part_id = $1 ORDER BY created_at ASC, id ASC"#,
        held_part,
    )
    .fetch_all(&_pool)
    .await
    .expect("query held events");
    assert!(
        held_events.iter().any(|e| e == "SENT_TO_INSPECTION"),
        "held_part 应有 SENT_TO_INSPECTION 事件: {held_events:?}"
    );
    // pool_part：TAKEN_FROM_POOL（refill 后写）
    let pool_events: Vec<String> = sqlx::query_scalar!(
        r#"SELECT event_type AS "event_type!"
        FROM t_part_event WHERE part_id = $1 ORDER BY created_at ASC, id ASC"#,
        pool_part,
    )
    .fetch_all(&_pool)
    .await
    .expect("query pool events");
    assert!(
        pool_events.iter().any(|e| e == "TAKEN_FROM_POOL"),
        "pool_part 应有 TAKEN_FROM_POOL 事件: {pool_events:?}"
    );
}

/// 场景 12: admin refill 端点
#[tokio::test]
async fn admin_refill_endpoint_works() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL12").await;
    let proc = seed_process(&pool, "PROC-L", "工序L").await;
    let wt = insert_work_type(&pool, "WT-L", "工种L", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-L", "PROD-L", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC012", "工12", Some(wt)).await;
    for i in 0..5 {
        let sn = format!("P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, prod_shelf, proc, 1).await;
    }

    let (app, token, _pool) = login_manager(pool.clone(), "admin12").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin refill: {env}");
    assert_eq!(env["code"], 0);
    let taken = env["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken.len(), 3, "max=3，应 taken=3: {env}");
    let held = count_held_by_worker(&_pool, worker).await;
    assert_eq!(held, 3);
}

/// 场景 13: admin_remove 把持有批次放回候选池
#[tokio::test]
async fn admin_remove_returns_batch_to_pool() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL13").await;
    let proc = seed_process(&pool, "PROC-M", "工序M").await;
    let wt = insert_work_type(&pool, "WT-M", "工种M", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-M", "PROD-M", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC013", "工13", Some(wt)).await;
    let (held_part, held_batch) =
        insert_worker_held_part(&pool, customer, "H-013", worker, proc, 1).await;

    let (app, token, _pool) = login_manager(pool.clone(), "admin13").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/remove",
            Some(json!({
                "worker_id": worker.to_string(),
                "batch_id": held_batch.to_string(),
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin remove: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["part_id"], held_part.to_string());
    assert_eq!(env["data"]["batch_id"], held_batch.to_string());

    // worker 应不再持有该批次（count=0）
    let held = count_held_by_worker(&_pool, worker).await;
    assert_eq!(held, 0, "admin_remove 后 worker 应释放该批次");
    // batch 应回到 PRODUCTION_SHELF holder=shelf
    let row = sqlx::query!(
        r#"SELECT location AS "loc!", current_holder_id AS "ch?", next_process_id AS "np?"
        FROM t_part_batch WHERE id = $1"#,
        held_batch,
    )
    .fetch_one(&pool)
    .await
    .expect("query batch");
    assert_eq!(row.loc, "PRODUCTION_SHELF");
    assert_eq!(row.ch, Some(prod_shelf));
    assert_eq!(row.np, Some(proc));
}

/// 场景 14: refill 失败回滚 worker-scan（`#[ignore]`：DB 故障注入缺基建）
#[tokio::test]
#[ignore = "需要 DB 故障注入（人为断网 / 临时约束 / DROP TABLE mid-tx）来制造 refill 失败而 scan 成功的窗口；当前 test harness 无法注入"]
async fn refill_failure_rolls_back_worker_scan() {
    // 设计意图：refill_for_worker 与 worker_scan_event 共享 handler 内
    // begin() 的同一事务，refill 抛错 → 事务自动回滚 → scan 的
    // RETURNED_TO_SHELF / SENT_TO_INSPECTION 事件日志 + 状态翻转都应一并撤销。
    // 测试需要一种可控方式让 refill 内部失败（其它分支成功），例如：
    //   1. 先把 work_type 的 process 映射删掉（清空 t_work_type_process）
    //      → refill 进入「BIZ_WORK_TYPE_NO_PROCESS_MAPPING」分支抛错；
    //      但 scan 部分已写入事件日志；
    //   2. 验证：events 表里应无该 part 的新事件日志；
    //      part.location / current_holder_id 应保持原状。
    //   实现这一窗口需要在 scan 与 refill 中间时点突变 work_type 映射，
    //   而当前 refill 流程在 worker_scan_event *之后*（handler 层）调用，
    //   中间插入 mutation 需要重构或并发 tx。
}

/// 场景 15: work_type.max_held_batches = NULL → 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET
#[tokio::test]
async fn max_held_null_returns_error() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL15").await;
    let proc = seed_process(&pool, "PROC-O", "工序O").await;
    let wt = insert_work_type(&pool, "WT-O", "工种O", None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-O", "PROD-O", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC015", "工15", Some(wt)).await;
    insert_pool_part(&pool, customer, "P-015", prod_shelf, proc, 1).await;

    let (app, token, _pool) = login_manager(pool.clone(), "admin15").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "max_held NULL 应 400 (BIZ 业务错默认 400): {env}"
    );
    assert_eq!(
        env["code"], 20904,
        "BIZ_WORK_TYPE_MAX_HELD_NOT_SET: {env}"
    );
}

/// 场景 16: worker.work_type_id = NULL → 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET (走不到)
/// 实际：worker.work_type_id = NULL → BIZ_WORKER_NO_WORK_TYPE (20206)
#[tokio::test]
async fn worker_no_work_type_returns_error() {
    let (_guard, pool) = setup().await;
    let customer = insert_customer_l2(&pool, "POOL16").await;
    let proc = seed_process(&pool, "PROC-P", "工序P").await;
    let wt = insert_work_type(&pool, "WT-P", "工种P", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = common::insert_shelf(&pool, "PROD-P", "PROD-P", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC016", "工16", None).await;
    insert_pool_part(&pool, customer, "P-016", prod_shelf, proc, 1).await;

    let (app, token, _pool) = login_manager(pool.clone(), "admin16").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/admin/worker-pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "无 work_type 应 400: {env}");
    assert_eq!(env["code"], 20206, "BIZ_WORKER_NO_WORK_TYPE: {env}");
}
