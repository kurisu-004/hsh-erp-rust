//! part 域集成测试 —— to-process 流
//!
//! 覆盖：
//!   - to-process：shelf_id / next_process_id 非数字拒绝（FAIL 分支参数 → 新必填字段）
//!   - to-process：INSPECTION happy path / 非 INSPECTION 状态拒绝
//!   - to-process partial-split：INSPECTION 批次 qty=10 → quantity=3，拆批
//!
//! ## 并行 / 认证
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。
//! 每个用例 INSPECTOR token（白名单）。

#[path = "common/mod.rs"]
mod common;

#[path = "part_api_helpers.rs"]
mod helpers;

use axum::http::StatusCode;
use serde_json::json;

use helpers::*;

// ===========================================================================
//  Tests
// ===========================================================================

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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "INSPECTION").await;
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": "abc",
                "next_process_id": "1",
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "INSPECTION").await;
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": "abc",
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20104);
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "INSPECTION").await;
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": next_proc.to_string(),
                "note": "test fail",
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
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
    let batch_id = insert_batch(&_pool, part_id, 1, 5, "PENDING").await;
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": next_proc.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20103);
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
    let v = batch_version(&_pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": next_proc.to_string(),
                "quantity": 3,
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
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