//! part 域集成测试 —— to_ship / to_inspection / to_process 流（to-XXX 重命名）
//!
//! 覆盖（设计 §6 / §10 / Phase F / F2）：
//!   1. 批量 to-ship happy path —— 3 个 INSPECTION 工单 → 200 / submitted=3 / failed=0
//!   2. 批量 to-ship partial failure —— 2 INSPECTION + 1 IN_PROCESS → 200 / submitted=2 / failed=1 (20103)
//!   3. 批量 to-ship 空 items 校验 —— 422 / 40001 VALIDATION_ERROR
//!   4. 批量 to-ship CLERK 越权 —— 403 / 40300 FORBIDDEN
//!   5. 单件 to-ship happy path —— POST /{part_id}/to-ship → 200 / part.status=READY_TO_SHIP
//!   6. 单件 to-ship OCC retry —— 第二次同件送检 → 400 / 20103 BIZ_INVALID_TRANSITION
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

async fn login_inspector(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "INSPECTOR", None, None).await;
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

async fn login_clerk(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "CLERK", None, None).await;
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

// ----- Fixture helpers（拷贝自 tests/delivery_scan_api.rs，按约定不跨文件复用） -----

async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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

/// to_inspection / to_process 测试用：插一个 INSPECTION 货架 + 一个 PRODUCTION 货架。
///
/// `next_process_id` 是 to_process 需要的占位（service 仅透传，不校验存在性——
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

/// 批量 to-ship happy path：3 个 INSPECTION 工单 → 200 / submitted=3 / failed=0。
#[tokio::test]
async fn batch_to_ship_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let mut bids = Vec::new();
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
        let bid = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
        bids.push(bid);
    }

    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "items": bids
            .iter()
            .map(|b| json!({"batch_id": b.to_string()}))
            .collect::<Vec<_>>()
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-ship",
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "happy path: {env}");
    assert_eq!(env["code"], 0);
    let submitted = env["data"]["submitted"].as_array().expect("data.submitted");
    let failed = env["data"]["failed"].as_array().expect("data.failed");
    assert_eq!(submitted.len(), 3, "应 submitted=3: {env}");
    assert_eq!(failed.len(), 0, "应 failed=0: {env}");
    // 全部都是 READY_TO_SHIP
    for s in submitted {
        assert_eq!(s["part"]["status"], "READY_TO_SHIP");
        assert_eq!(s["new_batch_id"], serde_json::Value::Null);
    }
}

/// 批量 to-ship partial failure：3 个工单（2 INSPECTION + 1 IN_PROCESS）→ 200 /
/// submitted=2 / failed=1 (code=20103)。
#[tokio::test]
async fn batch_to_ship_partial_failure() {
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
        let bid = insert_batch(&pool, pid, 1, 1, status).await;
        items.push(json!({"batch_id": bid.to_string()}));
    }

    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-ship",
            Some(json!({ "items": items })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "partial failure: {env}");
    assert_eq!(env["code"], 0);
    let submitted = env["data"]["submitted"].as_array().expect("data.submitted");
    let failed = env["data"]["failed"].as_array().expect("data.failed");
    assert_eq!(submitted.len(), 2, "应 submitted=2: {env}");
    assert_eq!(failed.len(), 1, "应 failed=1: {env}");
    assert_eq!(failed[0]["code"], 20103, "失败码应为 20103: {env}");
    // message 来自 AppError Display: "[20103] part <id> 当前状态 IN_PROCESS 不允许品检通过"
    let msg = failed[0]["message"].as_str().expect("failed.message");
    assert!(msg.starts_with("[20103]"), "message 应以 [20103] 开头: {msg}");
    assert!(msg.contains("IN_PROCESS"), "message 应含 IN_PROCESS: {msg}");
    // failed.batch_id 应当是 IN_PROCESS 的 P001 对应的 batch（i=1）
    let failed_bid = failed[0]["batch_id"].as_str().expect("failed.batch_id is string");
    let expected = items[1]["batch_id"].as_str().unwrap().to_string();
    assert_eq!(failed_bid, expected);
}

/// 批量 to-ship items=[] → 422 / 40001 VALIDATION_ERROR（handler 兜底校验）。
#[tokio::test]
async fn batch_to_ship_empty_items_40001() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-ship",
            Some(json!({"items": []})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "empty items: {env}");
    assert_eq!(env["code"], 40001);
}

/// 批量 to-ship item.batch_id 非数字 → 200 / submitted=0 / failed=1 (code=40001)。
///
/// 任务 4 review（Fix #3）：旧行为是把 `"abc"` 静默吞掉成 `None`，下游
/// `to_ship_core` 落到"找不到 INSPECTION 批次"分支报 `20109`，
/// 信息误导。新行为：service 在解析 batch_id 时就 push `40001 VALIDATION_ERROR`。
///
/// to-XXX 重命名后 BatchOpItem 不再含 `part_id`：service 从 `batch_id` 反查
/// part；无法 parse 的 batch_id 落到 `40001` 失败，sentinel `batch_id=0`。
#[tokio::test]
async fn batch_to_ship_non_numeric_batch_id_40001() {
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
            "/parts/batch-to-ship",
            Some(json!({
                "items": [{"batch_id": "abc"}]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "non-numeric batch_id: {env}");
    assert_eq!(env["code"], 0);
    let submitted = env["data"]["submitted"].as_array().expect("data.submitted");
    let failed = env["data"]["failed"].as_array().expect("data.failed");
    assert_eq!(submitted.len(), 0, "应 submitted=0: {env}");
    assert_eq!(failed.len(), 1, "应 failed=1: {env}");
    assert_eq!(
        failed[0]["code"], 40001,
        "non-numeric batch_id 应报 40001 VALIDATION_ERROR，而非 20109 BIZ_PART_BATCH_NOT_FOUND: {env}"
    );
    let msg = failed[0]["message"].as_str().expect("failed.message");
    assert!(msg.contains("abc"), "message 应含原值 'abc': {msg}");
    // failed.batch_id 应当是 sentinel 0（无法 parse 的 batch_id 不回填）
    let failed_bid = failed[0]["batch_id"].as_str().expect("failed.batch_id is string");
    assert_eq!(failed_bid, "0", "未 parse 的 batch_id 应 fallback 到 sentinel 0");
}

/// 批量 to-ship CLERK 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn batch_to_ship_clerk_forbidden() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_clerk(pool, "clerk1").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-ship",
            Some(json!({"items": [{"batch_id": "1"}]})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "clerk forbidden: {env}");
    assert_eq!(env["code"], 40300);
}

/// 单件 to-ship happy path：1 个 INSPECTION 工单 → 200 / part.status=READY_TO_SHIP。
///
/// 使用 INSPECTOR 角色验证「Manager 或 Inspector」白名单。
/// 响应 shape：ToXxxOut { part: PartOut, new_batch_id: Option<i64> }，
/// 整批操作时 new_batch_id=null。
#[tokio::test]
async fn single_to_ship_happy_path() {
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
            &format!("/parts/{pid}/to-ship"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "single happy: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["part"]["status"], "READY_TO_SHIP");
    assert_eq!(
        env["data"]["part"]["id"].as_str().unwrap().to_string(),
        pid.to_string()
    );
    assert_eq!(env["data"]["new_batch_id"], serde_json::Value::Null);
}

/// 单件 to-ship OCC retry：第二次送检 → 400 / 20103 BIZ_INVALID_TRANSITION
/// （READY_TO_SHIP → READY_TO_SHIP 被状态机白名单拒绝，不是 40901 VERSION_CONFLICT）。
///
/// 任务 3 review note：第二次送检走状态机守卫，状态机白名单不含
/// `READY_TO_SHIP → READY_TO_SHIP`，返回 20103 而非 40901。
#[tokio::test]
async fn single_to_ship_retry_returns_20103() {
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
            &format!("/parts/{pid}/to-ship"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first call: {env1}");
    assert_eq!(env1["data"]["part"]["status"], "READY_TO_SHIP");

    // 2nd call：状态机拒绝，20103 BIZ_INVALID_TRANSITION
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/to-ship"),
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
//  Phase F2 集成测试 —— to_inspection / batch-to-inspection / to_process
//
//  覆盖：
//   - to_inspection：happy path ×3（PENDING / PROGRAMMING / IN_PROCESS+PRODUCTION_SHELF）
//   - to_inspection：IN_PROCESS+WORKER / IN_PROCESS+非 PRODUCTION_SHELF holder 拒绝
//   - to_inspection：target shelf zone≠INSPECTION / is_active=false 拒绝
//   - to_process：shelf_id / next_process_id 非数字拒绝（FAIL 分支参数 → 新必填字段）
//   - batch-to-inspection：items 空 / 超 200 / 3 件混合 / CLERK 越权
//   - to_process：INSPECTION happy path / 非 INSPECTION 状态拒绝
// ===========================================================================

/// to-inspection happy path：PENDING → INSPECTION（无 PASS/FAIL 分支）。
///
/// to-XXX 重命名：原一键送检（带 PASS/FAIL 分支）已拆分为 to-inspection +
/// to-ship + to-process 三步；此用例只验证送检（to-inspection）单步。
#[tokio::test]
async fn to_inspection_from_pending_succeeds() {
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
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
    assert_eq!(body["data"]["part"]["id"], part_id.to_string());
    assert_eq!(body["data"]["new_batch_id"], serde_json::Value::Null);
}

/// to-inspection happy path：PROGRAMMING → INSPECTION。
#[tokio::test]
async fn to_inspection_from_programming_succeeds() {
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
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
}

/// to-inspection happy path：IN_PROCESS + PRODUCTION_SHELF holder → INSPECTION。
///
/// service 层组合校验：IN_PROCESS + 当前 holder 是 PRODUCTION 货架才放行。
#[tokio::test]
async fn to_inspection_from_in_process_production_shelf_succeeds() {
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
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
}

/// to-inspection 拒绝：IN_PROCESS + WORKER holder → 20103。
///
/// service 用 `ShelfRepo::get_by_id(current_holder_id)` 返回 None 启发式识别 worker 持有。
#[tokio::test]
async fn to_inspection_in_process_worker_rejected() {
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
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(body["message"].as_str().unwrap().contains("工人持有"));
}

/// to-inspection 拒绝：IN_PROCESS + 非 PRODUCTION_SHELF holder → 20103。
///
/// holder 是 INSPECTION 货架 → service 拒绝「不在生产架上」。
#[tokio::test]
async fn to_inspection_in_process_non_production_shelf_rejected() {
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
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(body["message"].as_str().unwrap().contains("不在生产架上"));
}

/// to-process 拒绝：shelf_id 非数字 → 20104 BIZ_INVALID_VALUE。
///
/// to-XXX 重命名：原一键送检的 FAIL 分支 `shelf_id` / `next_process_id` 在
/// `to-process` 成为必填字段（`ToProcessRequest.shelf_id: String`）。缺字段走
/// axum Json 提取 → 422 不在信封内（已退化为非 envelope plain text），故改用
/// 「非数字」值（"abc"）保留 service 层 20104 校验路径的覆盖。
#[tokio::test]
async fn to_process_invalid_shelf_id_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (_insp, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
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
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": "abc",
                "next_process_id": "1",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20104);
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains("abc"), "message 应含原值 'abc': {msg}");
}

/// to-process 拒绝：next_process_id 非数字 → 20104 BIZ_INVALID_VALUE。
#[tokio::test]
async fn to_process_invalid_next_process_id_rejected() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (_insp, prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
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
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": "abc",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20104);
}

/// to-inspection 拒绝：target_inspection_shelf.zone = PRODUCTION → 20511。
#[tokio::test]
async fn to_inspection_target_shelf_wrong_zone_rejected() {
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
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": prod_shelf.to_string(),  // 故意用 PRODUCTION 架
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20511);
}

/// to-inspection 拒绝：target_inspection_shelf.is_active = false → 20512。
#[tokio::test]
async fn to_inspection_target_shelf_inactive_rejected() {
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
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 400, "body={body}");
    assert_eq!(body["code"], 20512);
}

/// batch-to-inspection 拒绝：items 为空 → 422 / 40001 VALIDATION_ERROR。
#[tokio::test]
async fn batch_to_inspection_empty_items_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
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

/// batch-to-inspection 拒绝：items 数量 > 200 → 422 / 40001 VALIDATION_ERROR。
#[tokio::test]
async fn batch_to_inspection_too_many_items_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;
    let items: Vec<i64> = (1..=201).collect();
    let item_payloads: Vec<Value> = items
        .iter()
        .map(|id| json!({ "batch_id": id.to_string() }))
        .collect();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
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

/// batch-to-inspection 部分成功：3 件混合 → 2 submitted + 1 failed (20103)。
///
/// 测点：IN_PROCESS+fake_holder item 在 per-item 独立 core 中被 holder 守卫拒绝；
/// PENDING / PROGRAMMING item 走完 to-inspection 路径（status=INSPECTION）。
///
/// to-XXX 重命名后 BatchOpItem 用 `batch_id` 定位失败项，`part_id` 已删除。
#[tokio::test]
async fn batch_to_inspection_mixed_partial_success() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;

    // 第 1 件：PENDING → 应成功（status=INSPECTION）
    let p1 = insert_part_with_status(&_pool, "P1", l2, Some("P001"), None, "PENDING").await;
    let b1 = insert_batch(&_pool, p1, 1, 5, "PENDING").await;
    // 第 2 件：PROGRAMMING → 应成功
    let p2 = insert_part_with_status(&_pool, "P2", l2, Some("P002"), None, "PROGRAMMING").await;
    let b2 = insert_batch(&_pool, p2, 1, 5, "PROGRAMMING").await;
    // 第 3 件：IN_PROCESS + fake holder → 应失败 (20103)
    let p3 = insert_part_with_status(&_pool, "P3", l2, Some("P003"), None, "IN_PROCESS").await;
    let b3 = insert_batch(&_pool, p3, 1, 5, "IN_PROCESS").await;
    sqlx::query!("UPDATE t_part SET current_holder_id = 999999999 WHERE id = $1", p3)
        .execute(&_pool)
        .await
        .unwrap();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "items": [
                    { "batch_id": b1.to_string() },
                    { "batch_id": b2.to_string() },
                    { "batch_id": b3.to_string() },
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
    assert_eq!(body["data"]["failed"][0]["batch_id"], b3.to_string());
}

/// batch-to-inspection 权限：CLERK 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn batch_to_inspection_clerk_forbidden() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_clerk(pool, "clerk1").await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves(&_pool).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "items": [{ "batch_id": "1" }],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 403, "body={body}");
    assert_eq!(body["code"], 40300);
}

/// to-process happy path：INSPECTION → IN_PROCESS（推荐需求 3）。
#[tokio::test]
async fn to_process_happy_path() {
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
            &format!("/parts/{part_id}/to-process"),
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
    assert_eq!(body["data"]["part"]["status"], "IN_PROCESS");
}

/// to-process 拒绝：非 INSPECTION 状态（PENDING） → 400 / 20103。
#[tokio::test]
async fn to_process_wrong_state_rejected() {
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
            &format!("/parts/{part_id}/to-process"),
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

// ===========================================================================
//  Phase F3 partial-split 集成测试 —— quantity < batch.quantity 走拆批分支
//
//  to_ship / to_inspection / to_process 都支持「部分通过」：caller 传
//  `quantity < target.quantity` 时 service 拆出新批次 + remainder 留源状态，
//  响应 `new_batch_id = Some(remainder_id)`；缺省 / `quantity == target.quantity`
//  时不拆批，`new_batch_id = null`。
//
//  覆盖：
//   - to_ship partial-split：INSPECTION 批次 qty=10 → quantity=3，拆批
//   - to_inspection partial-split：PENDING 批次 qty=10 → quantity=3，拆批
//   - to_process partial-split：INSPECTION 批次 qty=10 → quantity=3，拆批
//   - to_ship full-batch：INSPECTION 批次 qty=10 → quantity=10（整批），不拆批
// ===========================================================================

/// to-ship partial-split happy path：INSPECTION 批次 qty=10 → quantity=3 → 拆批。
///
/// 期望：
/// - `new_batch_id` 非 null（remainder 留 INSPECTION）；
/// - `part.status` 保持 INSPECTION（rollup 守卫：剩 7 件仍在 INSPECTION，
///   部分通过整 part 不翻状态）。
/// - 响应 `part` 投影展示最新 OCC 版本。
#[tokio::test]
async fn to_ship_partial_split_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "INSPECTION",
    )
    .await;
    let batch_id = insert_batch(&_pool, part_id, 1, 10, "INSPECTION").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-ship"),
            Some(json!({
                "quantity": 3,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["code"], 0);
    // part.status 保持 INSPECTION（rollup 守卫：剩余 7 件还在 INSPECTION）
    assert_eq!(
        body["data"]["part"]["status"], "INSPECTION",
        "partial-split 后 part.status 应保持 INSPECTION（rollup 守卫检测到 remainder INSPECTION 批次）"
    );
    // 拆批后剩余批次 id（remainder 留在 INSPECTION 待后续操作）
    let new_bid_str = body["data"]["new_batch_id"]
        .as_str()
        .expect("new_batch_id 应为 string (Some)");
    assert_eq!(
        new_bid_str,
        batch_id.to_string(),
        "remainder id 应回填为源批次 id"
    );
}

/// to-inspection partial-split happy path：PENDING 批次 qty=10 → quantity=3 → 拆批。
///
/// 期望：`new_batch_id` 非 null（remainder 留 PENDING）；`part.status` 已翻转为 INSPECTION。
///
/// `split_batch_for_partial_pass` 现在通过 `new_batch_status` 参数接收源批次 status，
/// 因此 `to_inspection`（源 = `PENDING`）拆出的新批次以 `PENDING` 起始，能通过
/// `mark_batch_inspected` 的 WHERE 守卫。
#[tokio::test]
async fn to_inspection_partial_split_happy_path() {
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
    let batch_id = insert_batch(&_pool, part_id, 1, 10, "PENDING").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "quantity": 3,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
    let new_bid_str = body["data"]["new_batch_id"]
        .as_str()
        .expect("new_batch_id 应为 string (Some)");
    assert_eq!(
        new_bid_str,
        batch_id.to_string(),
        "remainder id 应回填为源批次 id"
    );
}

/// to-process partial-split happy path：INSPECTION 批次 qty=10 → quantity=3 → 拆批。
///
/// 期望：
/// - `new_batch_id` 非 null（remainder 留 INSPECTION）；
/// - `part.status` 保持 INSPECTION（rollup 守卫：剩 7 件仍在 INSPECTION，
///   部分打回整 part 不翻状态）。
/// - 响应 `part` 投影展示最新 OCC 版本。
#[tokio::test]
async fn to_process_partial_split_happy_path() {
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
    let batch_id = insert_batch(&_pool, part_id, 1, 10, "INSPECTION").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": next_proc.to_string(),
                "quantity": 3,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["code"], 0);
    // part.status 保持 INSPECTION（rollup 守卫：剩余 7 件还在 INSPECTION）
    assert_eq!(
        body["data"]["part"]["status"], "INSPECTION",
        "partial-split 后 part.status 应保持 INSPECTION（rollup 守卫检测到 remainder INSPECTION 批次）"
    );
    // 拆批后剩余批次 id（remainder 留在 INSPECTION 待后续操作）
    let new_bid_str = body["data"]["new_batch_id"]
        .as_str()
        .expect("new_batch_id 应为 string (Some)");
    assert_eq!(
        new_bid_str,
        batch_id.to_string(),
        "remainder id 应回填为源批次 id"
    );
}

/// to-ship full-batch：INSPECTION 批次 qty=10 → quantity 省略 → 不拆批。
///
/// 期望：`new_batch_id == null`（整批操作不触发 split_batch_for_partial_pass）。
#[tokio::test]
async fn to_ship_full_batch() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
    let part_id = insert_part_with_status(
        &_pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "INSPECTION",
    )
    .await;
    let _batch_id = insert_batch(&_pool, part_id, 1, 10, "INSPECTION").await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-ship"),
            Some(json!({})),  // quantity 缺省 = 整批
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["part"]["status"], "READY_TO_SHIP");
    assert_eq!(
        body["data"]["new_batch_id"],
        serde_json::Value::Null,
        "整批操作 new_batch_id 应为 null: {body}"
    );
}