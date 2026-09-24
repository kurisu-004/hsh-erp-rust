//! part 域集成测试 —— to-inspection / batch-to-inspection 流
//!
//! 覆盖：
//!   - to-inspection：happy path ×3（PENDING / PROGRAMMING / IN_PROCESS+PRODUCTION_SHELF）
//!   - to-inspection：IN_PROCESS+WORKER / IN_PROCESS+非 PRODUCTION_SHELF holder 拒绝
//!   - to-inspection：target shelf zone≠INSPECTION / is_active=false 拒绝
//!   - batch-to-inspection：items 空 / 超 200 / 3 件混合 / CLERK 越权
//!   - to-inspection partial-split：PENDING 批次 qty=10 → quantity=3，拆批
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//! 每个用例 INSPECTOR token（白名单）。

use axum::http::StatusCode;
use serde_json::{Value, json};
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

async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
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

/// to-inspection happy path：PENDING → INSPECTION（无 PASS/FAIL 分支）。
#[tokio::test]
async fn to_inspection_from_pending_succeeds() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
        5,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
    assert_eq!(body["data"]["part"]["id"], part_id.to_string());
    assert_eq!(body["data"]["new_batch_id"], serde_json::Value::Null);
}

/// to-inspection happy path：PROGRAMMING → INSPECTION。
#[tokio::test]
async fn to_inspection_from_programming_succeeds() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PROGRAMMING",
        5,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
}

/// to-inspection happy path：IN_PROCESS + PRODUCTION_SHELF holder → INSPECTION。
///
/// service 层组合校验：IN_PROCESS + 当前 holder 是 PRODUCTION 货架才放行。
#[tokio::test]
async fn to_inspection_from_in_process_production_shelf_succeeds() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "IN_PROCESS",
        5,
    )
    .await;
    sqlx::query(
        "UPDATE t_part_batch SET location = 'PRODUCTION_SHELF', current_holder_id = $1 \
         WHERE id = $2",
    )
    .bind(fx.production_shelf_id)
    .bind(batch_id)
    .execute(&pool)
    .await
    .unwrap();
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
}

/// to-inspection 拒绝：IN_PROCESS + WORKER holder → 20103。
///
/// service 用 `ShelfRepo::get_by_id(current_holder_id)` 返回 None 启发式识别 worker 持有。
#[tokio::test]
async fn to_inspection_in_process_worker_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "IN_PROCESS",
        5,
    )
    .await;
    let fake_holder: i64 = 999_999_999;
    sqlx::query(
        "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = $1 \
         WHERE id = $2",
    )
    .bind(fake_holder)
    .bind(batch_id)
    .execute(&pool)
    .await
    .unwrap();
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(body["message"].as_str().unwrap().contains("工人持有"));
}

/// to-inspection 拒绝：IN_PROCESS + 非 PRODUCTION_SHELF holder → 20103。
#[tokio::test]
async fn to_inspection_in_process_non_production_shelf_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // 第二个 INSPECTION 货架当 holder（让 part 持有一个非 PRODUCTION 的 shelf）
    let holder_shelf = insert_shelf(&pool, "INSP-002", "品检架B", "INSPECTION").await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "IN_PROCESS",
        5,
    )
    .await;
    sqlx::query(
        "UPDATE t_part_batch SET location = 'INSPECTION_SHELF', current_holder_id = $1 \
         WHERE id = $2",
    )
    .bind(holder_shelf)
    .bind(batch_id)
    .execute(&pool)
    .await
    .unwrap();
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(body["message"].as_str().unwrap().contains("不在生产架上"));
}

/// to-inspection 拒绝：target_inspection_shelf.zone = PRODUCTION → 20511。
#[tokio::test]
async fn to_inspection_target_shelf_wrong_zone_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
        5,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.production_shelf_id.to_string(),  // 故意用 PRODUCTION 架
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20511);
}

/// to-inspection 拒绝：target_inspection_shelf.is_active = false → 20512。
#[tokio::test]
async fn to_inspection_target_shelf_inactive_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // 把品检架置为 inactive
    sqlx::query!(
        "UPDATE t_shelf SET is_active = false WHERE id = $1",
        fx.inspection_shelf_id
    )
    .execute(&pool)
    .await
    .unwrap();
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
        5,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20512);
}

/// batch-to-inspection 拒绝：items 为空 → 422 / 40001 VALIDATION_ERROR。
#[tokio::test]
async fn batch_to_inspection_empty_items_rejected() {
    let (_pool, app, token, fx) = bootstrap_as_inspector().await;
    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "items": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(body["code"], 40001);
}

/// batch-to-inspection 拒绝：items 数量 > 200 → 422 / 40001 VALIDATION_ERROR。
#[tokio::test]
async fn batch_to_inspection_too_many_items_rejected() {
    let (_pool, app, token, fx) = bootstrap_as_inspector().await;
    let items: Vec<i64> = (1..=201).collect();
    let item_payloads: Vec<Value> = items
        .iter()
        .map(|id| json!({ "batch_id": id.to_string(), "version": 0 }))
        .collect();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "items": item_payloads,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(body["code"], 40001);
}

/// batch-to-inspection 部分成功：3 件混合 → 2 submitted + 1 failed (20103)。
#[tokio::test]
async fn batch_to_inspection_mixed_partial_success() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;

    // 第 1 件：PENDING → 应成功（status=INSPECTION）
    let (_p1, b1) = insert_part_with_batch(
        &pool,
        "P1",
        fx.customer_l2_id,
        Some("P001"),
        "PENDING",
        5,
    )
    .await;
    // 第 2 件：PROGRAMMING → 应成功
    let (_p2, b2) = insert_part_with_batch(
        &pool,
        "P2",
        fx.customer_l2_id,
        Some("P002"),
        "PROGRAMMING",
        5,
    )
    .await;
    // 第 3 件：IN_PROCESS + fake holder → 应失败 (20103)
    let (_p3, b3) = insert_part_with_batch(
        &pool,
        "P3",
        fx.customer_l2_id,
        Some("P003"),
        "IN_PROCESS",
        5,
    )
    .await;
    sqlx::query!(
        "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = 999999999 \
         WHERE id = $1",
        b3
    )
    .execute(&pool)
    .await
    .unwrap();
    let (v1, v2, v3) = (
        batch_version(&pool, b1).await,
        batch_version(&pool, b2).await,
        batch_version(&pool, b3).await,
    );

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "items": [
                    { "batch_id": b1.to_string(), "version": v1 },
                    { "batch_id": b2.to_string(), "version": v2 },
                    { "batch_id": b3.to_string(), "version": v3 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["submitted"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"]["failed"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["failed"][0]["code"], 20103);
    assert_eq!(body["data"]["failed"][0]["batch_id"], b3.to_string());
}

/// batch-to-inspection 权限：CLERK 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn batch_to_inspection_clerk_forbidden() {
    let (_pool, app, token, fx) = bootstrap_as_clerk().await;
    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "items": [{ "batch_id": "1", "version": 0 }],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body={body}");
    assert_eq!(body["code"], 40300);
}

/// to-inspection partial-split happy path：PENDING 批次 qty=10 → quantity=3 → 拆批。
#[tokio::test]
async fn to_inspection_partial_split_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
        10,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
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
    // PR-B2 §5 改造：part 派生列由 batch rollup 决定。
    assert_eq!(body["data"]["part"]["status"], "PENDING");
    // operated 子批（qty=3）应已翻转到 INSPECTION（DB 直查）
    let operated_status: String = sqlx::query_scalar(
        "SELECT status FROM t_part_batch WHERE part_id = $1 AND status = 'INSPECTION' AND deleted_at IS NULL",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .expect("read operated batch status");
    assert_eq!(
        operated_status, "INSPECTION",
        "operated 子批应已翻转到 INSPECTION"
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

// ===========================================================================
//  Scan context 端点（`GET /parts/by-serial/{serial_no}/part-batches`）
// ===========================================================================

/// Scan context happy path：PENDING → to-inspection → by-serial/part-batches → to-ship
/// 端到端验证。
#[tokio::test]
async fn part_batches_returns_narrow_part_and_batches_with_holder() {
    let (pool, app, insp_token, fx) = bootstrap_as_inspector().await;

    // 唯一 serial 避免并发 / 历史数据碰撞
    let serial_no = format!(
        "T-S-{:08}",
        chrono::Utc::now().timestamp_millis() % 100_000_000
    );

    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P-SCAN",
        fx.customer_l2_id,
        Some(&serial_no),
        "PENDING",
        5,
    )
    .await;
    let v_initial = batch_version(&pool, batch_id).await;

    // Step 1：INSPECTOR 触发 to-inspection
    let (status, body) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v_initial,
            })),
            Some(&insp_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "to-inspection: body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");

    // Step 2：复用同一 pool + fixture + app，直接换 MANAGER token
    // （再调 bootstrap_as_manager 会建新 DB，前面的 part 丢失）
    let mgr_token = login_token(
        &app,
        PartFixture::MANAGER_USERNAME,
        PartFixture::PASSWORD,
    )
    .await;
    let (status2, body2) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/parts/by-serial/{serial_no}/part-batches"),
            None::<Value>,
            Some(&mgr_token),
        ),
    )
    .await;
    assert_eq!(status2, StatusCode::OK, "scan-context: body={body2}");
    assert_eq!(body2["code"], 0);

    // ② 工单窄字段断言
    let part = &body2["data"]["part"];
    assert_eq!(part["id"], part_id.to_string(), "part.id 应等于 part_id");
    assert_eq!(part["drawing_no"], "D-001", "part.drawing_no 应为 'D-001'");
    assert!(
        part["customer_id"].is_string(),
        "customer_id 应序列化为 string: {part}"
    );
    assert_eq!(
        part["customer_id"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        fx.customer_l2_id,
        "customer_id 应等于 l2 id"
    );
    // PartScanInfoOut 不含 serial_no / status / version 等多余字段
    assert!(
        part.get("serial_no").is_none(),
        "scan-info 不应含 serial_no: {part}"
    );
    assert!(
        part.get("status").is_none(),
        "scan-info 不应含 status: {part}"
    );
    assert!(
        part.get("version").is_none(),
        "scan-info 不应含 version: {part}"
    );

    // ③ 批次数组断言：1 条 INSPECTION 批次 + 解析后的 holder_name
    let batches = body2["data"]["batches"]
        .as_array()
        .expect("batches 应为 array");
    assert_eq!(batches.len(), 1, "应仅有 1 条活跃批次: {batches:?}");
    assert_eq!(batches[0]["status"], "INSPECTION");
    assert_eq!(batches[0]["quantity"], 5);
    assert!(
        batches[0]["holder_name"].is_string(),
        "INSPECTION 阶段 holder_name 应为 Some: {batches:?}"
    );
    assert_eq!(
        batches[0]["holder_name"].as_str().unwrap(),
        "FX 检验架",
        "holder_name 应解析为品检架名称"
    );
    assert!(
        batches[0]["version"].as_i64().unwrap() > 0,
        "to-inspection 后 batch.version 应递增: {batches:?}"
    );
    let scan_batch_id_str = batches[0]["id"].as_str().expect("batches[0].id is string");
    let scan_batch_version = batches[0]["version"].as_i64().unwrap() as i32;

    // ④ 链式调 to-ship：扫码弹窗 → 品检通过（核心验收）
    let (status3, body3) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-ship"),
            Some(json!({
                "batch_id": scan_batch_id_str,
                "version": scan_batch_version,
                "quantity": serde_json::Value::Null,
            })),
            Some(&mgr_token),
        ),
    )
    .await;
    assert_eq!(
        status3,
        StatusCode::OK,
        "扫码弹窗 → to-ship 端到端应 200: body={body3}"
    );
    assert_eq!(body3["code"], 0);
    assert_eq!(
        body3["data"]["part"]["status"], "READY_TO_SHIP",
        "to-ship 后 part.status 应翻转为 READY_TO_SHIP: {body3}"
    );
}

/// Scan context not-found：serial 不存在 → 20101 BIZ_PART_NOT_FOUND。
#[tokio::test]
async fn part_batches_serial_not_found_returns_20101() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (status, body) = send(
        app,
        json_request(
            "GET",
            "/parts/by-serial/NOT-EXIST-SCAN-CONTEXT/part-batches",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "serial 不存在应映射 404: body={body}"
    );
    assert_eq!(body["code"], 20101, "BIZ_PART_NOT_FOUND: body={body}");
    let msg = body["message"].as_str().expect("message 应为 string");
    assert!(
        msg.contains("不存在"),
        "message 应含 '不存在'（与 Python 契约对齐）: {msg}"
    );
}

/// Scan context 角色守卫：ShelfAccount 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn part_batches_role_guard_rejects_unauthorized() {
    let (pool, _app, _token, fx) = bootstrap_as_manager().await;
    let serial_no = format!(
        "T-RG-{:08}",
        chrono::Utc::now().timestamp_millis() % 100_000_000
    );
    let (_pid, _bid) = insert_part_with_batch(
        &pool,
        "P-RG",
        fx.customer_l2_id,
        Some(&serial_no),
        "PENDING",
        1,
    )
    .await;

    // 用 fixture 的 SHELF_ACCOUNT 用户登录（合法登录但越权）
    let shelf_app = test_app(test_state(pool.clone()).await);
    let shelf_token = login_token(
        &shelf_app,
        PartFixture::SHELF_ACCOUNT_USERNAME,
        PartFixture::PASSWORD,
    )
    .await;
    let (status, body) = send(
        shelf_app,
        json_request(
            "GET",
            &format!("/parts/by-serial/{serial_no}/part-batches"),
            None::<Value>,
            Some(&shelf_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ShelfAccount 越权应 403: body={body}"
    );
    assert_eq!(body["code"], 40300, "FORBIDDEN: body={body}");
}