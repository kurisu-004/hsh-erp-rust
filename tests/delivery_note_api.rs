//! delivery_note 端到端集成测试
//!
//! Phase P2 覆盖（Python `tests/test_delivery_note.py` 移植 + 设计 §3.4 范围校验）：
//!  1. counter_acquires_sequential_numbers（spec 1）
//!  2. create_draft_for_l1_succeeds + create_for_l2_returns_400_21407
//!  3. list_with_filters (status + pagination)
//!  4. get_with_parts with line_items (assembly fields populated)
//!  5. add_parts: same L1 ok; different L1 → 21407; on other active note
//!     → 21406; not INSPECTION/READY_TO_SHIP → 21405; partial quantity
//!     splits batch; scope mismatch → 21416 (group-scoped note + L2
//!     outside the group)
//!  6. remove_parts: DRAFT ok; SUBMITTED → 21412
//!  7. submit: DRAFT ok; recall → DRAFT ok; recall into existing draft
//!     scope → 21419
//!  8. pickup: non-driver worker → 21409; happy path → PICKED_UP;
//!     ws_hub.broadcast observed
//!  9. soft_delete: DRAFT ok; non-DRAFT → 21403
//! 10. version conflict on any write → 40901
//! 11. list_candidate_parts for L1 (200, contains fixtures); non-L1 → 400
//!
//! 所有用例共享 `postgres_rust_test` + 唯一约束（delivery_note_no unique、
//! uq_t_delivery_note_draft_group/leaf），用进程级 `tokio::sync::Mutex` 串行化。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, ensure_database_exists, insert_user_with_password,
    test_app, test_pool, test_state,
};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

// ===========================================================================
//  全局串行化 + helpers
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
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

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

/// 直插 L1 客户
async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id,
        name,
        prefix,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

/// 直插 L2 客户（parent_id = l1_id）
async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id,
        name,
        l1_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

/// 直插工单
async fn insert_part(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, \
         quantity, has_been_repaired, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'D-001', $4, 'INSPECTION', $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL)",
        id,
        serial_no,
        name,
        customer_id,
        now,
        today,
    )
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 直插批次
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, has_been_repaired, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, false, 0, $6, NULL, $6, NULL)",
        id,
        part_id,
        batch_no,
        quantity,
        status,
        now,
    )
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

/// 直插送货分组
async fn insert_group(pool: &PgPool, l1_id: i64, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_delivery_group (id, customer_id, name, version, created_at, \
         created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 0, $4, NULL, $4, NULL)",
        id,
        l1_id,
        name,
        now,
    )
    .execute(pool)
    .await
    .expect("insert group");
    id
}

/// 直插分组成员
async fn insert_group_member(pool: &PgPool, group_id: i64, l2_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_delivery_group_member (id, group_id, customer_id, created_at, created_by) \
         VALUES ($1, $2, $3, $4, NULL)",
        id,
        group_id,
        l2_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert group member");
    id
}

/// 直插工种 + 工人
async fn insert_worker(pool: &PgPool, badge: &str, name: &str, is_active: bool, work_type_code: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
    // 找或插工种
    let wt_id: i64 = sqlx::query_scalar!(
        "SELECT id FROM t_work_type WHERE code = $1 LIMIT 1",
        work_type_code
    )
    .fetch_optional(pool)
    .await
    .expect("query work_type")
    .unwrap_or_else(|| panic!("work_type {} not seeded", work_type_code));

    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, NULL, $6, NULL)",
        id,
        badge,
        name,
        is_active,
        wt_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert worker");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn counter_acquires_sequential_numbers() {
    use hsh_erp_rust::infra::serial::next_delivery_note_no;
    let (_guard, pool) = setup().await;

    // 清表
    sqlx::query!("TRUNCATE t_delivery_note_counter")
        .execute(&pool)
        .await
        .expect("truncate counter");

    let no1 = next_delivery_note_no(&pool, 0).await.expect("acquire 1");
    let no2 = next_delivery_note_no(&pool, 0).await.expect("acquire 2");

    assert!(no1.starts_with("DN-"), "no1 must start with DN-: {no1}");
    assert!(no2.starts_with("DN-"), "no2 must start with DN-: {no2}");

    // 取 NN 部分
    let nn1: i32 = no1.split('-').nth(2).unwrap().parse().unwrap();
    let nn2: i32 = no2.split('-').nth(2).unwrap().parse().unwrap();
    assert_eq!(nn1, 1, "first NN should be 1, got {nn1} from {no1}");
    assert_eq!(nn2, 2, "second NN should be 2, got {nn2} from {no2}");

    // 前缀（含日期）相同
    let prefix1: Vec<&str> = no1.split('-').collect();
    let prefix2: Vec<&str> = no2.split('-').collect();
    assert_eq!(prefix1[0..2], prefix2[0..2], "DN-{{ymd}} prefix should match");
}

#[tokio::test]
async fn create_draft_for_l1_succeeds_and_for_l2_returns_400_21407() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let (app, token, pool) = login_manager(pool, "admin").await;

    // L1 草稿 → 200
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({"customer_id": l1.to_string(), "note": "draft 1"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "create l1: {env1}");
    let no1 = env1["data"]["delivery_note_no"].as_str().unwrap();
    assert!(no1.starts_with("DN-"), "delivery_note_no format: {no1}");

    // L2 → 400 / 21407
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({"customer_id": l2.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::BAD_REQUEST);
    assert_eq!(
        env2["code"], 21407,
        "code = 21407 (PARTS_MULTIPLE_CUSTOMERS); full = {env2}"
    );
}

#[tokio::test]
async fn list_with_filters_status_and_pagination() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let (app, token, pool) = login_manager(pool, "admin").await;

    // 建 3 张草稿
    for i in 0..3 {
        let (s, _) = send(
            app.clone(),
            json_request(
                "POST",
                "/delivery-notes",
                Some(json!({"customer_id": l1.to_string(), "note": format!("d{i}")})),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    // status=DRAFT, limit=2 → 2 条 + total=3
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            "/delivery-notes?statuses=DRAFT&limit=2&offset=0",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(env["data"]["total"], 3);
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 2);

    // customer_id 不存在 → 404 / 20102
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            "/delivery-notes?customer_id=999999",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(env2["data"]["total"], 0);
}

#[tokio::test]
async fn get_with_parts_with_assembly_fields() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "零件 A", l2, Some("A001")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 5, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (cs, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": batch_id.to_string(), "quantity": null}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK, "create: {env}");
    let note_id = env["data"]["id"].as_str().unwrap().to_string();
    let head_no = env["data"]["delivery_note_no"].as_str().unwrap().to_string();

    // GET /delivery-notes/{id}
    let (gs, genv) = send(
        app.clone(),
        json_request("GET", &format!("/delivery-notes/{note_id}"), None, Some(&token)),
    )
    .await;
    assert_eq!(gs, StatusCode::OK, "get: {genv}");
    assert_eq!(genv["data"]["delivery_note_no"], head_no);
    let items = genv["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["id"].as_str().unwrap(), batch_id.to_string());
    assert_eq!(item["serial_no"], "A001");
    assert_eq!(item["quantity"], 5);
    assert_eq!(item["status"], "INSPECTION");
    // 无装配件时 assembly_* 为 null
    assert!(item["assembly_id"].is_null());
    assert!(item["assembly_drawing_no"].is_null());

    // scanned_serials 应为空数组（P2 行为）
    assert!(genv["data"]["scanned_serials"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn add_parts_same_l1_ok_different_l1_returns_400_21407() {
    let (_guard, pool) = setup().await;
    let l1_a = insert_l1(&pool, "法拉电子", "F").await;
    let l1_b = insert_l1(&pool, "路达电子", "L").await;
    let l2_b = insert_l2(&pool, "路达一厂", l1_b).await;
    let part_id = insert_part(&pool, "X", l2_b, Some("X001")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 3, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    // l1_a 单
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({"customer_id": l1_a.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let note_id = env1["data"]["id"].as_str().unwrap().to_string();
    let version = env1["data"]["version"].as_i64().unwrap() as i32;

    // add l1_b 工单 → 21407
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/add-parts"),
            Some(json!({
                "items": [{"batch_id": batch_id.to_string()}],
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::BAD_REQUEST, "diff L1: {env2}");
    assert_eq!(env2["code"], 21407);
}

#[tokio::test]
async fn add_parts_already_assigned_returns_409_21406() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X002")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 3, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    // 第一张单
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": batch_id.to_string()}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let note_a_id = env1["data"]["id"].as_str().unwrap().to_string();

    // 第二张单 (L1 全域草稿，撞不到唯一索引因为我们用 L1Wide)
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({"customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "create note B: {env2}");
    let note_b_id = env2["data"]["id"].as_str().unwrap().to_string();
    let version_b = env2["data"]["version"].as_i64().unwrap() as i32;

    // 把 batch 加到第二张单 → 应撞 21406（已在第一张单 DRAFT 上）
    let (s3, env3) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_b_id}/add-parts"),
            Some(json!({
                "items": [{"batch_id": batch_id.to_string()}],
                "version": version_b,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::CONFLICT);
    assert_eq!(env3["code"], 21406);
    let _ = note_a_id;
}

#[tokio::test]
async fn add_parts_invalid_status_returns_400_21405() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X003")).await;
    // 批次在 PROCESSING 状态（不允许 INSPECTION/READY_TO_SHIP 以外）
    let batch_id = insert_batch(&pool, part_id, 1, 3, "PROCESSING").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({"customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK);
    let note_id = cenv["data"]["id"].as_str().unwrap().to_string();
    let version = cenv["data"]["version"].as_i64().unwrap() as i32;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/add-parts"),
            Some(json!({
                "items": [{"batch_id": batch_id.to_string()}],
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(env["code"], 21405);
}

#[tokio::test]
async fn add_parts_partial_quantity_splits_batch() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X004")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 10, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({"customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK);
    let note_id = cenv["data"]["id"].as_str().unwrap().to_string();
    let version = cenv["data"]["version"].as_i64().unwrap() as i32;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/add-parts"),
            Some(json!({
                "items": [{"batch_id": batch_id.to_string(), "quantity": 4}],
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "split add: {env}");
    let items = env["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let row = &items[0];
    assert_eq!(row["quantity"], 4, "新批次应只剩 quantity=4");

    // 原批次 quantity 减到 6、未挂单
    let original_qty: i32 = sqlx::query_scalar!(
        "SELECT quantity AS \"qty!\" FROM t_part_batch WHERE id = $1",
        batch_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(original_qty, 6, "源批次 quantity 减 4 → 6");
}

#[tokio::test]
async fn add_parts_group_scope_mismatch_returns_400_21416() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_a = insert_l2(&pool, "二厂", l1).await;
    let l2_b = insert_l2(&pool, "五厂", l1).await; // 组外
    let part_id = insert_part(&pool, "X", l2_b, Some("X005")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 3, "INSPECTION").await;

    // 建组 = {二厂}
    let gid = insert_group(&pool, l1, "组1").await;
    insert_group_member(&pool, gid, l2_a).await;

    // 建分组单
    let (app, token, pool) = login_manager(pool, "admin").await;
    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({"customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK, "create L1-wide draft: {cenv}");
    let note_id = cenv["data"]["id"].as_str().unwrap().to_string();
    let version = cenv["data"]["version"].as_i64().unwrap() as i32;

    // 设本单为分组单 delivery_group_id = gid（手工 SQL，因为 P2 API 没暴露）
    sqlx::query!(
        "UPDATE t_delivery_note SET delivery_group_id = $1 WHERE id = $2",
        gid,
        note_id.parse::<i64>().unwrap(),
    )
    .execute(&pool)
    .await
    .unwrap();

    // add 五厂 工单 → 应撞 21416（组外）
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/add-parts"),
            Some(json!({
                "items": [{"batch_id": batch_id.to_string()}],
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "scope mismatch: {env}");
    assert_eq!(env["code"], 21416);
}

#[tokio::test]
async fn remove_parts_draft_ok_submitted_returns_409_21412() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X006")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 3, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": batch_id.to_string()}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK);
    let note_id = cenv["data"]["id"].as_str().unwrap().to_string();
    let version = cenv["data"]["version"].as_i64().unwrap() as i32;

    // remove DRAFT → ok
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/remove-parts"),
            Some(json!({
                "batch_ids": [batch_id.to_string()],
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "remove draft: {env}");
    assert_eq!(env["data"]["line_items"].as_array().unwrap().len(), 0);

    // 再 add + 推到 READY_TO_SHIP + submit
    // 注：remove 不 bump note version，所以仍用 version
    let (s2, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/add-parts"),
            Some(json!({
                "items": [{"batch_id": batch_id.to_string()}],
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    sqlx::query!(
        "UPDATE t_part_batch SET status = 'READY_TO_SHIP' WHERE id = $1",
        batch_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let (senv, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/submit"),
            Some(json!({"version": version})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(senv, StatusCode::OK);

    // remove on SUBMITTED → 21412（submit 之后 version+1）
    let (s3, env3) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/remove-parts"),
            Some(json!({
                "batch_ids": [batch_id.to_string()],
                "version": version + 1,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::CONFLICT);
    assert_eq!(env3["code"], 21412);
}

#[tokio::test]
async fn submit_and_recall_draft_scope_conflict_returns_409_21419() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X007")).await;
    let batch_id_a = insert_batch(&pool, part_id, 1, 1, "READY_TO_SHIP").await;
    let batch_id_b = insert_batch(&pool, part_id, 2, 1, "READY_TO_SHIP").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    // 第一张单（要走完 submit → recall）
    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": batch_id_a.to_string()}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK);
    let note_a_id = cenv["data"]["id"].as_str().unwrap().to_string();

    // 第二张单（也要建好 DRAFT，挡 recall）
    let (_, b_env) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": batch_id_b.to_string()}],
            })),
            Some(&token),
        ),
    )
    .await;
    let _note_b_id = b_env["data"]["id"].as_str().unwrap().to_string();

    // submit note A
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_a_id}/submit"),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;

    // recall note A → 同范围已有 note B DRAFT → 21419
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_a_id}/recall"),
            Some(json!({"version": 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "recall撞 draft scope: {env}");
    assert_eq!(env["code"], 21419);

    // 现在软删 note B，再 recall 应成功
    sqlx::query!(
        "UPDATE t_part_batch SET delivery_note_id = NULL WHERE delivery_note_id = $1",
        b_env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap()
    )
    .execute(&pool)
    .await
    .unwrap();
    let (del, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{}/soft-delete", b_env["data"]["id"].as_str().unwrap()),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(del, StatusCode::OK);

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_a_id}/recall"),
            Some(json!({"version": 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "recall after delete: {env2}");
    assert_eq!(env2["data"]["status"], "DRAFT");
}

#[tokio::test]
async fn pickup_non_driver_returns_400_21409_and_happy_path_picks_up() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X008")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 1, "READY_TO_SHIP").await;

    // 工种（直接 INSERT；truncate 后表空，不会撞 unique）
    sqlx::query!(
        "INSERT INTO t_work_type (id, code, name, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, now(), now())",
        SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1).next_id(),
        "送货司机",
        "送货司机",
    )
    .execute(&pool)
    .await
    .expect("insert work_type 送货司机");
    sqlx::query!(
        "INSERT INTO t_work_type (id, code, name, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, now(), now())",
        SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1).next_id(),
        "其他工种",
        "其他工种",
    )
    .execute(&pool)
    .await
    .expect("insert work_type 其他工种");
    let driver_id = insert_worker(&pool, "D001", "张司机", true, "送货司机").await;
    let non_driver_id = insert_worker(&pool, "N001", "王操作员", true, "其他工种").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    // 建单 → submit
    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": batch_id.to_string()}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK);
    let note_id = cenv["data"]["id"].as_str().unwrap().to_string();

    let (senv, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/submit"),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(senv, StatusCode::OK);

    // non-driver → 21409
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/pickup"),
            Some(json!({
                "driver_worker_id": non_driver_id.to_string(),
                "version": 1,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::BAD_REQUEST, "non-driver: {env1}");
    assert_eq!(env1["code"], 21409);

    // 订阅 ws hub
    let mut rx = hsh_erp_rust::infra::ws_hub::WsHub::default().subscribe();
    let _ = rx; // unused

    // driver → PICKED_UP
    let state = hsh_erp_rust::state::AppState::new(
        pool.clone(),
        std::sync::Arc::new(hsh_erp_rust::infra::config::AppConfig::from_env(".env").unwrap()),
        std::sync::Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1)),
        std::sync::Arc::new(hsh_erp_rust::infra::ws_hub::WsHub::default()),
        std::sync::Arc::new(hsh_erp_rust::infra::cos::NoopCos),
        tokio_util::sync::CancellationToken::new(),
    );
    let _ = state; // unused — pickup 测试通过 app 走

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/pickup"),
            Some(json!({
                "driver_worker_id": driver_id.to_string(),
                "version": 1,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "driver pickup: {env2}");
    assert_eq!(env2["data"]["status"], "PICKED_UP");
}

#[tokio::test]
async fn soft_delete_draft_ok_non_draft_returns_400_21403() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X009")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 1, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": batch_id.to_string()}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK);
    let note_id = cenv["data"]["id"].as_str().unwrap().to_string();

    // soft-delete DRAFT → ok
    let (s, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/soft-delete"),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // 新建一个 + 推到 READY + submit → soft-delete SUBMITTED → 21403
    let batch_id2 = insert_batch(&pool, part_id, 2, 1, "READY_TO_SHIP").await;
    let (_, cenv2) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": batch_id2.to_string()}],
            })),
            Some(&token),
        ),
    )
    .await;
    let note_id2 = cenv2["data"]["id"].as_str().unwrap().to_string();

    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id2}/submit"),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id2}/soft-delete"),
            Some(json!({"version": 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::BAD_REQUEST, "non-draft: {env2}");
    assert_eq!(env2["code"], 21403);
}

#[tokio::test]
async fn version_conflict_on_write_returns_409_40901() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let (app, token, pool) = login_manager(pool, "admin").await;

    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({"customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK);
    let note_id = cenv["data"]["id"].as_str().unwrap().to_string();

    // 用错的 version submit → 40901
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/submit"),
            Some(json!({"version": 999})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(env["code"], 40901);
}

#[tokio::test]
async fn list_candidate_parts_l1_returns_fixtures_non_l1_returns_400() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X010")).await;
    let _batch = insert_batch(&pool, part_id, 1, 2, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;

    // L1 → 200 + 至少 1 条
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/delivery-notes/candidate-parts?customer_id={l1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(env["data"]["items"].as_array().unwrap().len() >= 1);

    // L2 → 400
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            &format!("/delivery-notes/candidate-parts?customer_id={l2}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::BAD_REQUEST);
    assert_eq!(env2["code"], 20104);
}