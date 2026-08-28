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
                "items": [{ "batch_id": "1" }],
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