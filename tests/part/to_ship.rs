//! part 域集成测试 —— to_ship / batch-to-ship 流
//!
//! 覆盖：
//!   1. 批量 to-ship happy path —— 3 个 INSPECTION 工单 → 200 / submitted=3 / failed=0
//!   2. 批量 to-ship partial failure —— 2 INSPECTION + 1 IN_PROCESS → 200 / submitted=2 / failed=1 (20103)
//!   3. 批量 to-ship 空 items 校验 —— 422 / 40001 VALIDATION_ERROR
//!   4. 批量 to-ship CLERK 越权 —— 403 / 40300 FORBIDDEN
//!   5. 单件 to-ship happy path —— POST /{part_id}/to-ship → 200 / part.status=READY_TO_SHIP
//!   6. 单件 to-ship OCC retry —— 第二次同件送检 → 400 / 20103 BIZ_INVALID_TRANSITION
//!      （READY_TO_SHIP → READY_TO_SHIP 被状态机白名单拒绝，不是 40901 VERSION_CONFLICT）
//!   7. 批量 to-ship item.batch_id 非数字 → 200 / submitted=0 / failed=1 (code=40001)
//!   8. to-ship partial-split：INSPECTION 批次 qty=10 → quantity=3，拆批
//!   9. to-ship full-batch：INSPECTION 批次 qty=10 → quantity 省略 → 不拆批
//!  10. 单件 to-ship 落后 version → 409 / 40901 VERSION_CONFLICT
//!  11. 批量 to-ship 落后 version → 该 item 落 failed[40901]，兄弟 item 仍 submitted
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//! 每个用例 MANAGER 或 INSPECTOR token。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::*;

// ===========================================================================
//  动态 part/batch 插入 helper（sub-file 私有，PR-C 末统一迁）
// ===========================================================================

async fn insert_part_with_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    status: &str,
    qty: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let batch_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-001', $4, $7, $3, $5, $5, 1, 0, $6, $6)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, $4, 0, $5, $5)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(qty)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    (part_id, batch_id)
}

async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
}

// ===========================================================================
//  bootstrap helpers
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_clerk() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 批量 to-ship happy path：3 个 INSPECTION 工单 → 200 / submitted=3 / failed=0。
#[tokio::test]
async fn batch_to_ship_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let mut bids = Vec::new();
    for i in 0..3 {
        let (_pid, bid) = insert_part_with_batch(
            &pool,
            &format!("P{i}"),
            fx.customer_l2_id,
            Some(&format!("P{i:03}")),
            "INSPECTION",
            1,
        )
        .await;
        bids.push(bid);
    }

    let mut items = Vec::new();
    for b in &bids {
        let v = batch_version(&pool, *b).await;
        items.push(json!({"batch_id": b.to_string(), "version": v}));
    }
    let body = json!({ "items": items });
    let (s, env) = send(
        app,
        json_request("POST", "/parts/batch-to-ship", Some(body), Some(&token)),
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
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let mut items = Vec::new();
    for i in 0..3 {
        let status = if i == 1 { "IN_PROCESS" } else { "INSPECTION" };
        let (_pid, bid) = insert_part_with_batch(
            &pool,
            &format!("P{i}"),
            fx.customer_l2_id,
            Some(&format!("P{i:03}")),
            status,
            1,
        )
        .await;
        let v = batch_version(&pool, bid).await;
        items.push(json!({"batch_id": bid.to_string(), "version": v}));
    }

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
    assert!(
        msg.starts_with("[20103]"),
        "message 应以 [20103] 开头: {msg}"
    );
    assert!(msg.contains("IN_PROCESS"), "message 应含 IN_PROCESS: {msg}");
}

/// 批量 to-ship items=[] → 422 / 40001 VALIDATION_ERROR（handler 兜底校验）。
#[tokio::test]
async fn batch_to_ship_empty_items_40001() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 准备一个 valid part + batch（不参与调用，只为触发 service 真正跑解析）
    let (_pid, _bid) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        1,
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-ship",
            Some(json!({
                "items": [{"batch_id": "abc", "version": 0}]
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
    let failed_bid = failed[0]["batch_id"]
        .as_str()
        .expect("failed.batch_id is string");
    assert_eq!(
        failed_bid, "0",
        "未 parse 的 batch_id 应 fallback 到 sentinel 0"
    );
}

/// 批量 to-ship CLERK 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn batch_to_ship_clerk_forbidden() {
    let (_pool, app, token, _fx) = bootstrap_as_clerk().await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-ship",
            Some(json!({"items": [{"batch_id": "1", "version": 0}]})),
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
#[tokio::test]
async fn single_to_ship_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (pid, bid) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        1,
    )
    .await;
    let v = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/to-ship"),
            Some(json!({"batch_id": bid.to_string(), "version": v})),
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
#[tokio::test]
async fn single_to_ship_retry_returns_20103() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        1,
    )
    .await;
    let v = batch_version(&pool, bid).await;

    // 1st call：happy path
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{pid}/to-ship"),
            Some(json!({"batch_id": bid.to_string(), "version": v})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first call: {env1}");
    assert_eq!(env1["data"]["part"]["status"], "READY_TO_SHIP");

    // 2nd call：状态机拒绝，20103 BIZ_INVALID_TRANSITION
    let v2 = batch_version(&pool, bid).await;
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/to-ship"),
            Some(json!({"batch_id": bid.to_string(), "version": v2})),
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

/// to-ship partial-split happy path：INSPECTION 批次 qty=10 → quantity=3 → 拆批。
#[tokio::test]
async fn to_ship_partial_split_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        10,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-ship"),
            Some(json!({
                "batch_id": batch_id.to_string(),
                "version": v,
                "quantity": 3,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(
        body["data"]["part"]["status"], "INSPECTION",
        "partial-split 后 part.status 应保持 INSPECTION（rollup 守卫检测到 remainder INSPECTION 批次）"
    );
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
#[tokio::test]
async fn to_ship_full_batch() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        10,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-ship"),
            Some(json!({"batch_id": batch_id.to_string(), "version": v})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["part"]["status"], "READY_TO_SHIP");
    assert_eq!(
        body["data"]["new_batch_id"],
        serde_json::Value::Null,
        "整批操作 new_batch_id 应为 null: {body}"
    );
}

/// 单件 to-ship：caller 传的 batch version 落后 → 409 / 40901 VERSION_CONFLICT。
#[tokio::test]
async fn single_to_ship_stale_version_returns_409_40901() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        5,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-ship"),
            Some(json!({"batch_id": batch_id.to_string(), "version": v - 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "stale version: {body}");
    assert_eq!(body["code"], 40901, "stale version 应报 40901: {body}");
}

/// 批量 to-ship：一个 item 传落后 version → 落 `failed[]` (40901)，
/// 同批另一个 version 正确的 item 仍进 `submitted[]`。
#[tokio::test]
async fn batch_to_ship_stale_version_lands_in_failed() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;

    // item 1：version 正确 → submitted
    let (_p1, b1) = insert_part_with_batch(
        &pool,
        "P1",
        fx.customer_l2_id,
        Some("P001"),
        "INSPECTION",
        5,
    )
    .await;
    let v1 = batch_version(&pool, b1).await;
    // item 2：version 故意落后一版 → failed[40901]
    let (_p2, b2) = insert_part_with_batch(
        &pool,
        "P2",
        fx.customer_l2_id,
        Some("P002"),
        "INSPECTION",
        5,
    )
    .await;
    let v2 = batch_version(&pool, b2).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-ship",
            Some(json!({
                "items": [
                    {"batch_id": b1.to_string(), "version": v1},
                    {"batch_id": b2.to_string(), "version": v2 - 1},
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "batch stale version: {body}");
    assert_eq!(body["code"], 0);
    let submitted = body["data"]["submitted"]
        .as_array()
        .expect("data.submitted");
    let failed = body["data"]["failed"].as_array().expect("data.failed");
    assert_eq!(
        failed.len(),
        1,
        "落后 version 的 item 必须落 failed[]: {body}"
    );
    assert_eq!(failed[0]["code"], 40901, "失败码应为 40901: {body}");
    assert_eq!(failed[0]["batch_id"], b2.to_string());
    assert_eq!(
        submitted.len(),
        1,
        "version 正确的 item 应 submitted: {body}"
    );
    assert_eq!(submitted[0]["part"]["status"], "READY_TO_SHIP");

    // savepoint 回滚校验：失败 item 的批次仍留在 INSPECTION，version 未被撞
    let b2_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM t_part_batch WHERE id = $1")
            .bind(b2)
            .fetch_one(&pool)
            .await
            .expect("b2");
    assert_eq!(b2_status, "INSPECTION", "失败 item 应被 savepoint 回滚");
}