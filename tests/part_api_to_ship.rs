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
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。
//! 每个用例 MANAGER 或 INSPECTOR token。

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
    let mut items = Vec::new();
    for b in &bids {
        let v = batch_version(&_pool, *b).await;
        items.push(json!({"batch_id": b.to_string(), "version": v}));
    }
    let body = json!({ "items": items });
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
        let v = batch_version(&pool, bid).await;
        items.push(json!({"batch_id": bid.to_string(), "version": v}));
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
    let bid = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
    let v = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
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
    let bid = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
    let v = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_manager(pool, "admin").await;

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
    // 注意：core 里状态机守卫（步骤 2）在 batch OCC 断言（步骤 3.5）之前，
    // 因此这里带上最新 version 也仍然是 20103，而不是 40901。
    let v2 = batch_version(&_pool, bid).await;
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
    let v = batch_version(&_pool, batch_id).await;

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
    let v = batch_version(&_pool, _batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-ship"),
            // quantity 缺省 = 整批
            Some(json!({"batch_id": _batch_id.to_string(), "version": v})),
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
///
/// 覆盖 `to_ship_core` 步骤 3.5 的 `_assert_batch_version`：状态机守卫已通过
/// （part 处于 INSPECTION），失败点必须是 batch 级 OCC 而非 20103。
#[tokio::test]
async fn single_to_ship_stale_version_returns_409_40901() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "INSPECTION").await;
    let batch_id = insert_batch(&pool, part_id, 1, 5, "INSPECTION").await;
    let v = batch_version(&pool, batch_id).await;

    let (app, token, _pool) = login_inspector(pool, "inspector1").await;
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
///
/// **回归护栏**：如果 `batch_to_ship` 把 `item.version` 换成
/// `target.version`（DB 读出来的值），比对恒等成立、OCC 静默失效，
/// 本用例会因 `failed.len() == 0` 而失败。
#[tokio::test]
async fn batch_to_ship_stale_version_lands_in_failed() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    // item 1：version 正确 → submitted
    let p1 = insert_part_with_status(&pool, "P1", l2, Some("P001"), None, "INSPECTION").await;
    let b1 = insert_batch(&pool, p1, 1, 5, "INSPECTION").await;
    let v1 = batch_version(&pool, b1).await;
    // item 2：version 故意落后一版 → failed[40901]
    let p2 = insert_part_with_status(&pool, "P2", l2, Some("P002"), None, "INSPECTION").await;
    let b2 = insert_batch(&pool, p2, 1, 5, "INSPECTION").await;
    let v2 = batch_version(&pool, b2).await;

    let (app, token, _pool) = login_manager(pool, "admin").await;
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
    let submitted = body["data"]["submitted"].as_array().expect("data.submitted");
    let failed = body["data"]["failed"].as_array().expect("data.failed");
    assert_eq!(
        failed.len(),
        1,
        "落后 version 的 item 必须落 failed[]（若聚合器传 target.version 则 OCC 静默失效）: {body}"
    );
    assert_eq!(failed[0]["code"], 40901, "失败码应为 40901: {body}");
    assert_eq!(failed[0]["batch_id"], b2.to_string());
    assert_eq!(submitted.len(), 1, "version 正确的 item 应 submitted: {body}");
    assert_eq!(submitted[0]["part"]["status"], "READY_TO_SHIP");

    // savepoint 回滚校验：失败 item 的批次仍留在 INSPECTION，version 未被撞
    let b2_status = sqlx::query_scalar::<_, String>("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(b2)
        .fetch_one(&_pool)
        .await
        .expect("b2");
    assert_eq!(b2_status, "INSPECTION", "失败 item 应被 savepoint 回滚");
}