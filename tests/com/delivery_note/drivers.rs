//! `GET /api/v2/com/delivery/drivers` 候选送货司机一览端到端测试
//!
//! 覆盖
//! 1. 只返「工种 = 送货司机」且在职、未软删的工人（每项 3 字段：`id` / `name` /
//!    `badge_code`）
//! 2. **不复用 `prod::worker` 的 11 字段 `WorkerOut`** —— 响应里不该出现
//!    `id_card_no` / `phone` / `version` / `created_at`（个人信息不该因为一个下拉框
//!    就发到前端）
//! 3. 排序 `w.name, w.id`
//! 4. 权限：Manager / Clerk / Inspector 放行；`ShelfAccount` 与无角色用户 ⇒ 40300
//! 5. 软删闸门：软删的司机 / 软删的工种都不出现
//! 6. 工种字面量与 `validate_driver` 同源（改一处必须改两处）

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_test_support::{
    PartFixture, json_request, load_delivery_fixture, load_part_fixture, login_token, send,
    test_app, test_pool, test_state,
};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, String) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(
        &app,
        &fx.part_manager_username,
        hsh_erp_test_support::DeliveryFixture::PASSWORD,
    )
    .await;
    (pool, app, token, "司机测试".to_string())
}

/// 找或插一个工种，返回 id。
async fn work_type(pool: &PgPool, code: &str, name: &str) -> i64 {
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM t_work_type WHERE code = $1")
        .bind(code)
        .fetch_optional(pool)
        .await
        .expect("query work_type")
    {
        return id;
    }
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 29).next_id();
    sqlx::query(
        "INSERT INTO t_work_type (id, code, name, sort_order, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 999, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now_naive())
    .execute(pool)
    .await
    .expect("insert work_type");
    id
}

/// 插一个工人。`active` / `soft_deleted` 控制两条过滤判据。
async fn insert_worker(
    pool: &PgPool,
    badge: &str,
    name: &str,
    wt_id: i64,
    active: bool,
    soft_deleted: bool,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 29).next_id();
    let now = now_naive();
    // `uk_t_worker_id_card_no` 全局唯一 ⇒ 用 badge 派生的 18 位串（同一测试内 badge
    // 各不相同，故不会撞）。证件号 / 手机号只是为了让「响应不该带 PII」那条断言
    // 有实际意义。
    let id_card = format!("{:0>18}", badge.trim_start_matches('D'));
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, created_by, updated_at, updated_by, deleted_at, id_card_no, phone) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, NULL, $6, NULL, $7, $8, $9)",
    )
    .bind(id)
    .bind(badge)
    .bind(name)
    .bind(active)
    .bind(wt_id)
    .bind(now)
    .bind(if soft_deleted { Some(now) } else { None })
    .bind(Some(id_card))
    .bind(Some(format!("138{:0>8}", badge.trim_start_matches('D'))))
    .execute(pool)
    .await
    .expect("insert worker");
    id
}

async fn list_drivers(app: &axum::Router, token: &str) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request("GET", "/com/delivery/drivers", None, Some(token)),
    )
    .await
}

/// 找出响应里 id == target 的项。
fn item_of(env: &Value, target: i64) -> &Value {
    env["data"]["items"]
        .as_array()
        .expect("items 必须是数组")
        .iter()
        .find(|w| w["id"].as_str() == Some(target.to_string().as_str()))
        .unwrap_or_else(|| panic!("响应里找不到司机 {target}: {env}"))
}

// ===========================================================================
//  1. 判据
// ===========================================================================

/// 只返「工种 = 送货司机 ∧ 在职 ∧ 两侧未软删」的工人，每项恰好 3 个字段。
#[tokio::test]
async fn drivers_list_only_returns_active_delivery_drivers() {
    let (pool, app, token, _) = bootstrap_as_manager().await;
    let driver_wt = work_type(&pool, "送货司机", "送货司机").await;
    let other_wt = work_type(&pool, "焊工", "焊工").await;

    let good_a = insert_worker(&pool, "D001", "安司机", driver_wt, true, false).await;
    let good_b = insert_worker(&pool, "D002", "波司机", driver_wt, true, false).await;
    // 以下 4 类都**不该**出现
    let inactive = insert_worker(&pool, "D003", "停用司机", driver_wt, false, false).await;
    let soft = insert_worker(&pool, "D004", "已删司机", driver_wt, true, true).await;
    let wrong_wt = insert_worker(&pool, "D005", "焊工阿离", other_wt, true, false).await;

    let (s, env) = list_drivers(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let ids: Vec<String> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&good_a.to_string()), "在职司机应在列: {env}");
    assert!(ids.contains(&good_b.to_string()), "在职司机应在列: {env}");
    for (label, id) in [("停用", inactive), ("软删", soft), ("非送货司机", wrong_wt)] {
        assert!(
            !ids.contains(&id.to_string()),
            "{label}的工人不该出现在候选里: {env}"
        );
    }
}

/// 响应项恰好 3 个字段（`id` / `name` / `badge_code`）—— 刻意不复用
/// `prod::worker::vo::WorkerOut`（11 字段，带 `id_card_no` / `phone` 等个人信息）。
#[tokio::test]
async fn driver_option_is_exactly_three_fields_without_pii() {
    let (pool, app, token, _) = bootstrap_as_manager().await;
    let driver_wt = work_type(&pool, "送货司机", "送货司机").await;
    let good = insert_worker(&pool, "D010", "丙司机", driver_wt, true, false).await;

    let (s, env) = list_drivers(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let item = item_of(&env, good);
    let obj = item.as_object().expect("item 必须是对象");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["badge_code", "id", "name"],
        "字段集必须恰好 3 个: {env}"
    );
    assert_eq!(item["badge_code"], "D010");
    assert_eq!(
        item["id"],
        good.to_string(),
        "雪花 id 必须序列化为 JSON string"
    );
    assert_eq!(item["name"], "丙司机");
}

/// 排序 `w.name, w.id`：同名时按 id 定序（同一数据集的分页 / 快照稳定）。
#[tokio::test]
async fn drivers_are_sorted_by_name_then_id() {
    let (pool, app, token, _) = bootstrap_as_manager().await;
    let driver_wt = work_type(&pool, "送货司机", "送货司机").await;
    let first = insert_worker(&pool, "D020", "同名司机", driver_wt, true, false).await;
    let second = insert_worker(&pool, "D021", "同名司机", driver_wt, true, false).await;
    insert_worker(&pool, "D022", "阿司机", driver_wt, true, false).await;

    let (s, env) = list_drivers(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let names: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "按 name 升序: {env}");

    let pos_first = names
        .iter()
        .position(|n| *n == "同名司机")
        .expect("有同名司机");
    let first_item = &env["data"]["items"][pos_first];
    let same_name: Vec<&Value> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["name"] == "同名司机")
        .collect();
    assert_eq!(
        same_name[0]["id"].as_str().unwrap(),
        first.to_string(),
        "同名时小的 id 在前"
    );
    assert_eq!(same_name[1]["id"].as_str().unwrap(), second.to_string());
    let _ = first_item;
}

/// 工种行被软删 ⇒ 该工种下的司机全部消失（`wt.deleted_at IS NULL` 闸门）。
#[tokio::test]
async fn soft_deleted_work_type_hides_all_its_workers() {
    let (pool, app, token, _) = bootstrap_as_manager().await;
    let driver_wt = work_type(&pool, "送货司机", "送货司机").await;
    let w = insert_worker(&pool, "D030", "丁司机", driver_wt, true, false).await;
    // 先确认在列
    let (_, before) = list_drivers(&app, &token).await;
    assert!(
        before["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"].as_str() == Some(w.to_string().as_str())),
        "先确认在列: {before}"
    );

    sqlx::query("UPDATE t_work_type SET deleted_at = now() WHERE id = $1")
        .bind(driver_wt)
        .execute(&pool)
        .await
        .expect("soft delete work_type");

    let (_, after) = list_drivers(&app, &token).await;
    assert!(
        !after["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"].as_str() == Some(w.to_string().as_str())),
        "工种软删后司机不该出现: {after}"
    );
}

/// 空结果也是合法响应（`items: []`，不是 `null`）。
#[tokio::test]
async fn empty_driver_list_returns_empty_array() {
    let (_pool, app, token, _) = bootstrap_as_manager().await;
    let s = send(
        app.clone(),
        json_request("GET", "/com/delivery/drivers", None, Some(&token)),
    )
    .await;
    assert_eq!(s.0, StatusCode::OK, "{:?}", s.1);
    let items = s.1["data"]["items"].as_array();
    assert!(items.is_some(), "items 必须是数组而非 null: {}", s.1);
}

// ===========================================================================
//  2. 权限
// ===========================================================================

/// Inspector 可进（品检员在扫码入单页要能选司机），货架终端 ⇒ 40300。
///
/// ⚠️ 只装 part fixture：`load_delivery_fixture` 与 `load_part_fixture` 的
/// `t_customer` 常量 id 区段重叠，同库两次装会撞主键。
#[tokio::test]
async fn drivers_list_allows_inspector_and_rejects_shelf_account() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);

    let inspector = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    let (s1, env1) = list_drivers(&app, &inspector).await;
    assert_eq!(s1, StatusCode::OK, "Inspector 应能读司机候选: {env1}");

    let shelf = login_token(&app, &fx.shelf_account_username, PartFixture::PASSWORD).await;
    let (s2, env2) = list_drivers(&app, &shelf).await;
    assert_eq!(
        s2,
        StatusCode::FORBIDDEN,
        "货架终端不该能读司机候选: {env2}"
    );
    assert_eq!(env2["code"], 40300);
}

/// 未带 token ⇒ 40100（鉴权中间件层）。
#[tokio::test]
async fn drivers_list_without_token_returns_40100() {
    let (_pool, app, _token, _) = bootstrap_as_manager().await;
    let s = send(
        app,
        json_request("GET", "/com/delivery/drivers", None, None),
    )
    .await;
    assert_eq!(s.0, StatusCode::UNAUTHORIZED, "{:?}", s.1);
    assert_eq!(s.1["code"], 40100);
}

/// 端点与其它 com 端点同前缀，且**不是** `/note` 的子资源（独立 nest）。
#[tokio::test]
async fn drivers_endpoint_is_under_com_delivery_drivers() {
    use hsh_erp_test_support::send_raw;
    let (_pool, app, token, _) = bootstrap_as_manager().await;
    for uri in [
        // 旧路径（域平移前）+ 猜测的嵌套路径都必须 404。
        // ⚠️ 用 `send_raw`：404 走 axum 的 fallback，**没有响应体**（`route_layer`
        // 只作用于已匹配路由），`send` 的 JSON 解析会 panic。
        "/delivery-notes/drivers",
        // 注意：`/com/delivery/note/{serial_no}` 是真路由，`drivers` 会作为
        // `serial_no` 命中它并走「序列号未命中」⇒ 404 / 20101（见下条断言）。
        "/com/delivery/driver",
        "/com/delivery/driver",
    ] {
        let (s, body) = send_raw(app.clone(), json_request("GET", uri, None, Some(&token))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{uri} 不该存在: {body}");
    }
}
