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

/// scan-inspect / fail-inspection 测试用：插一个 INSPECTION 货架 + 一个 PRODUCTION 货架。
///
/// `next_process_id` 是 fail-inspection 需要的占位（service 仅透传，不校验存在性——
/// shelf 域 `t_shelf_process` 映射校验留待后续 shelf 域 PR）。
async fn setup_inspection_and_production_shelves(pool: &PgPool) -> (i64, i64, i64) {
    use common::insert_shelf;
    let inspection_shelf = insert_shelf(pool, "INSP-001", "品检架A", "INSPECTION").await;
    let production_shelf = insert_shelf(pool, "PROD-001", "生产架A", "PRODUCTION").await;
    let next_process_id = 999_999_i64;
    (inspection_shelf, production_shelf, next_process_id)
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

/// 批量送检 item.batch_id 非数字 → 200 / passed=0 / failed=1 (code=40001)。
///
/// 任务 4 review（Fix #3）：旧行为是把 `"abc"` 静默吞掉成 `None`，下游
/// `pass_inspection_core` 落到"找不到 INSPECTION 批次"分支报 `20109`，
/// 信息误导。新行为：service 在解析 batch_id 时就 push `40001 VALIDATION_ERROR`。
#[tokio::test]
async fn batch_pass_inspection_non_numeric_batch_id_40001() {
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
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-pass-inspection",
            Some(json!({
                "items": [{"part_id": pid.to_string(), "batch_id": "abc"}]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "non-numeric batch_id: {env}");
    assert_eq!(env["code"], 0);
    let passed = env["data"]["passed"].as_array().expect("data.passed");
    let failed = env["data"]["failed"].as_array().expect("data.failed");
    assert_eq!(passed.len(), 0, "应 passed=0: {env}");
    assert_eq!(failed.len(), 1, "应 failed=1: {env}");
    assert_eq!(
        failed[0]["code"], 40001,
        "non-numeric batch_id 应报 40001 VALIDATION_ERROR，而非 20109 BIZ_PART_BATCH_NOT_FOUND: {env}"
    );
    let msg = failed[0]["message"].as_str().expect("failed.message");
    assert!(msg.contains("abc"), "message 应含原值 'abc': {msg}");
    let failed_pid = failed[0]["part_id"].as_str().expect("failed.part_id is string");
    assert_eq!(failed_pid, pid.to_string());
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

// ===========================================================================
//  Phase F2 集成测试 —— scan-inspect / batch-scan-inspect / fail-inspection
//
//  覆盖：
//   - scan-inspect：happy path ×3（PENDING / PROGRAMMING / IN_PROCESS+PRODUCTION_SHELF）
//   - scan-inspect：IN_PROCESS+WORKER / IN_PROCESS+非 PRODUCTION_SHELF holder 拒绝
//   - scan-inspect：FAIL 缺 shelf_id / next_process_id 拒绝
//   - scan-inspect：target shelf zone≠INSPECTION / is_active=false 拒绝
//   - batch-scan-inspect：items 空 / 超 200 / 3 件混合 / CLERK 越权
//   - fail-inspection：INSPECTION happy path / 非 INSPECTION 状态拒绝
// ===========================================================================

/// scan-inspect happy path：PENDING + PASS → READY_TO_SHIP。
#[tokio::test]
async fn scan_inspect_pending_pass_succeeds() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "PENDING").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "decision": "PASS",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["status"], "READY_TO_SHIP");
    assert_eq!(body["data"]["id"], part_id.to_string());
}

/// scan-inspect happy path：PROGRAMMING + PASS → READY_TO_SHIP。
#[tokio::test]
async fn scan_inspect_programming_pass_succeeds() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PROGRAMMING",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "PROGRAMMING").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "decision": "PASS",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["data"]["status"], "READY_TO_SHIP");
}

/// scan-inspect happy path：IN_PROCESS + PRODUCTION_SHELF holder + PASS → READY_TO_SHIP。
///
/// service 层组合校验：IN_PROCESS + 当前 holder 是 PRODUCTION 货架才放行。
#[tokio::test]
async fn scan_inspect_in_process_production_shelf_pass_succeeds() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "IN_PROCESS",
    )
    .await;
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "IN_PROCESS").await;
    // 把 part.current_holder_id 设为 prod_shelf（让 IN_PROCESS 路径通过组合校验）
    sqlx::query!(
        "UPDATE t_part SET current_holder_id = $1 WHERE id = $2",
        prod_shelf,
        part_id
    )
    .execute(&_pool)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE t_part_batch SET current_holder_id = $1 WHERE id = $2",
        prod_shelf,
        batch_id
    )
    .execute(&_pool)
    .await
    .unwrap();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "decision": "PASS",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["data"]["status"], "READY_TO_SHIP");
}

/// scan-inspect 拒绝：IN_PROCESS + WORKER holder → 20103。
///
/// service 用 `ShelfRepo::get_by_id(current_holder_id)` 返回 None 启发式识别 worker 持有。
#[tokio::test]
async fn scan_inspect_in_process_worker_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "IN_PROCESS",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "IN_PROCESS").await;
    // current_holder_id 指向一个不存在的 id（模拟 worker 持有）
    let fake_holder: i64 = 999_999_999;
    sqlx::query!(
        "UPDATE t_part SET current_holder_id = $1 WHERE id = $2",
        fake_holder,
        part_id
    )
    .execute(&_pool)
    .await
    .unwrap();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "decision": "PASS",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(body["message"].as_str().unwrap().contains("工人持有"));
}

/// scan-inspect 拒绝：IN_PROCESS + 非 PRODUCTION_SHELF holder → 20103。
///
/// holder 是 INSPECTION 货架 → service 拒绝「不在生产架上」。
#[tokio::test]
async fn scan_inspect_in_process_non_production_shelf_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    // 第二个 INSPECTION 货架当 holder（让 part 持有一个非 PRODUCTION 的 shelf）
    let holder_shelf = common::insert_shelf(&_pool, "INSP-002", "品检架B", "INSPECTION").await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "IN_PROCESS",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "IN_PROCESS").await;
    sqlx::query!(
        "UPDATE t_part SET current_holder_id = $1 WHERE id = $2",
        holder_shelf,
        part_id
    )
    .execute(&_pool)
    .await
    .unwrap();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "decision": "PASS",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(body["message"].as_str().unwrap().contains("不在生产架上"));
}

/// scan-inspect 拒绝：FAIL 缺 shelf_id → 20104 BIZ_INVALID_VALUE。
#[tokio::test]
async fn scan_inspect_fail_missing_shelf_id_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "PENDING").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "decision": "FAIL",
                // shelf_id 故意省略
                "next_process_id": "1",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20104);
}

/// scan-inspect 拒绝：FAIL 缺 next_process_id → 20104 BIZ_INVALID_VALUE。
#[tokio::test]
async fn scan_inspect_fail_missing_next_process_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "PENDING").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "decision": "FAIL",
                "shelf_id": prod_shelf.to_string(),
                // next_process_id 故意省略
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20104);
}

/// scan-inspect 拒绝：target_inspection_shelf.zone = PRODUCTION → 20511。
#[tokio::test]
async fn scan_inspect_target_shelf_wrong_zone_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (_insp_shelf, prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "PENDING").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": prod_shelf.to_string(),  // 故意用 PRODUCTION 架
                "decision": "PASS",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20511);
}

/// scan-inspect 拒绝：target_inspection_shelf.is_active = false → 20512。
#[tokio::test]
async fn scan_inspect_target_shelf_inactive_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    // 把品检架置为 inactive
    sqlx::query!("UPDATE t_shelf SET is_active = false WHERE id = $1", insp_shelf)
        .execute(&_pool)
        .await
        .unwrap();
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "PENDING").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/scan-inspect"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "decision": "PASS",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20512);
}

/// batch-scan-inspect 拒绝：items 为空 → 422 / 40001 VALIDATION_ERROR。
#[tokio::test]
async fn batch_scan_inspect_empty_items_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-scan-inspect",
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "items": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 422, "body={body}");
    assert_eq!(body["code"], 40001);
}

/// batch-scan-inspect 拒绝：items 数量 > 200 → 422 / 40001 VALIDATION_ERROR。
#[tokio::test]
async fn batch_scan_inspect_too_many_items_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let items: Vec<i64> = (1..=201).collect();
    let item_payloads: Vec<Value> = items
        .iter()
        .map(|id| json!({ "part_id": id.to_string() }))
        .collect();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-scan-inspect",
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "items": item_payloads,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 422, "body={body}");
    assert_eq!(body["code"], 40001);
}

/// batch-scan-inspect 部分成功：3 件混合 → 2 submitted + 1 failed (20103)。
///
/// 测点：IN_PROCESS+fake_holder item 在 per-item 独立 core 中被 holder 守卫拒绝；
/// PENDING / PROGRAMMING item 走完 PASS 路径。
#[tokio::test]
async fn batch_scan_inspect_mixed_partial_success() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;

    // 第 1 件：PENDING → 应成功（status=READY_TO_SHIP）
    let p1 = insert_part_with_status(&_pool, "P1", l2, Some("P001"), None, "PENDING").await;
    insert_batch(&_pool, p1, 1, 5, "PENDING").await;
    // 第 2 件：PROGRAMMING → 应成功
    let p2 = insert_part_with_status(&_pool, "P2", l2, Some("P002"), None, "PROGRAMMING").await;
    insert_batch(&_pool, p2, 1, 5, "PROGRAMMING").await;
    // 第 3 件：IN_PROCESS + fake holder → 应失败 (20103)
    let p3 = insert_part_with_status(&_pool, "P3", l2, Some("P003"), None, "IN_PROCESS").await;
    insert_batch(&_pool, p3, 1, 5, "IN_PROCESS").await;
    sqlx::query!("UPDATE t_part SET current_holder_id = 999999999 WHERE id = $1", p3)
        .execute(&_pool)
        .await
        .unwrap();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-scan-inspect",
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "items": [
                    { "part_id": p1.to_string() },
                    { "part_id": p2.to_string() },
                    { "part_id": p3.to_string() },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["submitted"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"]["failed"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["failed"][0]["code"], 20103);
    assert_eq!(body["data"]["failed"][0]["part_id"], p3.to_string());
}

/// batch-scan-inspect 权限：CLERK 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn batch_scan_inspect_clerk_forbidden() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_clerk(pool, "clerk1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-scan-inspect",
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "items": [{ "part_id": "1" }],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 403, "body={body}");
    assert_eq!(body["code"], 40300);
}

/// fail-inspection happy path：INSPECTION → IN_PROCESS（推荐需求 3）。
#[tokio::test]
async fn fail_inspection_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (_insp, prod_shelf, next_proc) = setup_inspection_and_production_shelves(&_pool).await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "INSPECTION",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "INSPECTION").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/fail-inspection"),
            Some(json!({
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": next_proc.to_string(),
                "note": "test fail",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["data"]["status"], "IN_PROCESS");
}

/// fail-inspection 拒绝：非 INSPECTION 状态（PENDING） → 400 / 20103。
#[tokio::test]
async fn fail_inspection_wrong_state_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (_insp, prod_shelf, next_proc) = setup_inspection_and_production_shelves(&_pool).await;
    // setup: PENDING part（非 INSPECTION）
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;
    insert_batch(&_pool, part_id, 1, 5, "PENDING").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/fail-inspection"),
            Some(json!({
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": next_proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20103);
}