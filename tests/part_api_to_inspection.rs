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
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。
//! 每个用例 INSPECTOR token（白名单）。

#[path = "common/mod.rs"]
mod common;

#[path = "part_api_helpers.rs"]
mod helpers;

use axum::http::StatusCode;
use serde_json::{json, Value};

use helpers::*;

// ===========================================================================
//  Tests
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "PENDING").await;
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "PROGRAMMING").await;
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
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
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "IN_PROCESS").await;
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
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "IN_PROCESS").await;
    sqlx::query!(
        "UPDATE t_part SET current_holder_id = $1 WHERE id = $2",
        holder_shelf,
        part_id
    )
    .execute(&_pool)
    .await
    .unwrap();
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "PENDING").await;
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": prod_shelf.to_string(),  // 故意用 PRODUCTION 架
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "PENDING").await;
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
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
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
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
        .map(|id| json!({ "batch_id": id.to_string(), "version": 0 }))
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
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
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
    let (v1, v2, v3) = (
        batch_version(&_pool, b1).await,
        batch_version(&_pool, b2).await,
        batch_version(&_pool, b3).await,
    );

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
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
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
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

// ===========================================================================
//  Tests — Scan context 端点（`GET /parts/by-serial/{serial_no}/part-batches`）
//
//  覆盖：
//    1. happy path → 窄字段工单 + 1 条 INSPECTION 批次（holder 解析为品检架名）；
//       并用返回的 batch_id/version 链式调 `POST /parts/{id}/to-ship` 验证
//       「扫码弹窗 → 品检通过」端到端。
//    2. serial 不存在 → 20101 BIZ_PART_NOT_FOUND，message 含「不存在」。
//    3. 角色守卫：ShelfAccount 越权 → 40300 FORBIDDEN
//       （LIST_PART_ROLES = Manager / Clerk / Inspector / CncProgrammer，
//        ShelfAccount 不在白名单内）。
//
//  角色守卫约定：本仓库 5 角色（Manager / Clerk / Inspector / CncProgrammer /
//  ShelfAccount）；brief 原话「Worker role」并不存在，follow-up 落实用
//  ShelfAccount（唯一不在 LIST_PART_ROLES 中的角色），与 worker_pool_api.rs
//  `pool_by_process_forbidden_for_shelf_account` 同形。
// ===========================================================================

/// Scan context happy path：PENDING → to-inspection → by-serial/part-batches → to-ship
/// 端到端验证。
///
/// 步骤：
///   1. 建 PENDING 工单（serial_no 唯一）+ PENDING 批次
///   2. INSPECTOR 调 to-inspection → 工单 INSPECTION、批次挪到品检架
///   3. MANAGER 调 GET /parts/by-serial/{serial}/part-batches
///      → 断言 part 窄字段（id / drawing_no / customer_id，无 serial_no/status/version 噪音）
///      → 断言 batches[0]：status="INSPECTION"、holder_name=Some("品检架A")、version>0
///   4. 用 batches[0].id/version 调 POST /parts/{pid}/to-ship
///      → 断言 200 + part.status="READY_TO_SHIP"（核心验收：扫码弹窗 → 品检通过）
#[tokio::test]
async fn part_batches_returns_narrow_part_and_batches_with_holder() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    // 唯一 serial 避免并发 / 历史数据碰撞；t_part.serial_no 是 varchar(15)
    // 所以用 8 位 millis 后缀 + 短前缀（`T-S-` = 12 chars 总长）。
    let serial_no = format!(
        "T-S-{:08}",
        chrono::Utc::now().timestamp_millis() % 100_000_000
    );

    let part_id = insert_part_with_status(
        &pool,
        "P-SCAN",
        l2,
        Some(&serial_no),
        None,
        "PENDING",
    )
    .await;
    let batch_id = insert_batch(&pool, part_id, 1, 5, "PENDING").await;
    let v_initial = batch_version(&pool, batch_id).await;

    // Step 1：INSPECTOR 触发 to-inspection
    let (app, insp_token, _pool) = login_inspector(pool, "inspector_scan").await;
    let (insp_shelf, _prod_shelf, _proc) =
        setup_inspection_and_production_shelves(&_pool).await;
    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v_initial,
            })),
            Some(&insp_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "to-inspection: body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");

    // Step 2：MANAGER 调 scan-context 端点
    let (app2, mgr_token, _pool2) = login_manager(_pool, "manager_scan").await;
    let (status2, body2) = send(
        app2.clone(),
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
    // insert_part_with_status hardcode drawing_no='D-001'（沿用既有 helper 行为）
    assert_eq!(part["drawing_no"], "D-001", "part.drawing_no 应为 'D-001'");
    // customer_id 是 i64，应序列化为 JSON string
    assert!(
        part["customer_id"].is_string(),
        "customer_id 应序列化为 string: {part}"
    );
    assert_eq!(
        part["customer_id"].as_str().unwrap().parse::<i64>().unwrap(),
        l2,
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
    // holder_name 由 list_active_by_part_id_with_holder 解析为 shelf.name="品检架A"
    assert!(
        batches[0]["holder_name"].is_string(),
        "INSPECTION 阶段 holder_name 应为 Some: {batches:?}"
    );
    assert_eq!(
        batches[0]["holder_name"].as_str().unwrap(),
        "品检架A",
        "holder_name 应解析为品检架名称"
    );
    // version 应 > 0（to-inspection 触发 batch OCC 递增）
    assert!(
        batches[0]["version"].as_i64().unwrap() > 0,
        "to-inspection 后 batch.version 应递增: {batches:?}"
    );
    let scan_batch_id_str = batches[0]["id"].as_str().expect("batches[0].id is string");
    let scan_batch_version = batches[0]["version"].as_i64().unwrap() as i32;

    // ④ 链式调 to-ship：扫码弹窗 → 品检通过（核心验收）
    let (status3, body3) = send(
        app2.clone(),
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
        body3["data"]["part"]["status"],
        "READY_TO_SHIP",
        "to-ship 后 part.status 应翻转为 READY_TO_SHIP: {body3}"
    );
}

/// Scan context not-found：serial 不存在 → 20101 BIZ_PART_NOT_FOUND。
///
/// 测点：service `get_part_batches_by_serial` 找不到 part 时抛
/// `code::BIZ_PART_NOT_FOUND (20101)`，message 形如 `serial_no XXX 不存在`。
#[tokio::test]
async fn part_batches_serial_not_found_returns_20101() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "manager_scan_miss").await;

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
    assert_eq!(
        body["code"],
        20101,
        "BIZ_PART_NOT_FOUND: body={body}"
    );
    let msg = body["message"].as_str().expect("message 应为 string");
    assert!(
        msg.contains("不存在"),
        "message 应含 '不存在'（与 Python 契约对齐）: {msg}"
    );
}

/// Scan context 角色守卫：ShelfAccount 越权 → 403 / 40300 FORBIDDEN。
///
/// endpoint 用 `LIST_PART_ROLES = Manager / Clerk / Inspector / CncProgrammer`，
/// ShelfAccount 不在白名单 —— service `require_any_role` 直接拒。
///
/// 内联 `login_shelf_account`（不引 worker_pool_api.rs 的私有 helper）；
/// pattern 与 `tests/worker_pool_api.rs::pool_by_process_forbidden_for_shelf_account` 同形。
#[tokio::test]
async fn part_batches_role_guard_rejects_unauthorized() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let serial_no = format!(
        "T-RG-{:08}",
        chrono::Utc::now().timestamp_millis() % 100_000_000
    );
    let _part_id = insert_part_with_status(
        &pool,
        "P-RG",
        l2,
        Some(&serial_no),
        None,
        "PENDING",
    )
    .await;

    // inline shelf_account login（mirror worker_pool_api.rs::login_shelf_account）
    let uid = common::insert_user_with_password(&pool, "shelf_user_scan", "changeme").await;
    let insp_shelf = common::insert_shelf(&pool, "INSP-RG", "品检架RG", "INSPECTION").await;
    common::add_role(&pool, uid, "SHELF_ACCOUNT", Some("shelf"), Some(insp_shelf)).await;
    let state = common::test_state(pool.clone()).await;
    let app = common::test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": "shelf_user_scan", "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = common::test_app(state);

    let (status, body) = send(
        app2,
        json_request(
            "GET",
            &format!("/parts/by-serial/{serial_no}/part-batches"),
            None::<Value>,
            Some(&token),
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