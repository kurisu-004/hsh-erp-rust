//! delivery-notes/scan 扫码入单端到端集成测试
//!
//! Phase P3 覆盖（设计 §5 / §10）：
//!   1. 散件首次扫码 → 200 ADDED + added_batches=1 + line_count=1
//!   2. 散件同码重复扫码 → 200 ALREADY_PRESENT（幂等）
//!   3. 未知码 → 404 / 21417
//!   4. 散件 status=IN_PROCESS → 400 / 21405（pre-route-b 旧行为；新行为见 Task 7）
//!   5. 散件已挂在其它 active 单 → 409 / 21406（note no 在 message）
//!   6. 装配件整套所有子件 READY → 200 ADDED + N 子件一批
//!   7. 装配件整套拒绝：1 子件 IN_PROCESS → 400 / 21418 + failures 含该子件（pre-route-b）
//!   8. 装配件整套重扫 → 200 ALREADY_PRESENT
//!   9. 同 L1 下跨 L2 自动分类：L2_in_group → 分组单，L2_out_group → 单厂单
//!  10. 无分组 L1：跨 L2 累加到同一草稿（L1Wide）
//!
//! 加场景（轻量）：scan 空码 → 400 / 20104。
//!
//! ## Task 7：scan-route-b 5 类状态分组 + 21421 + 幂等（7 场景）
//!
//! 上述 `scan_part_in_process_returns_400_21405` / `scan_assembly_atomic_reject_with_failures`
//! / `scan_assembly_full_all_subparts_ready_added` / `scan_part_on_other_active_note_returns_409_21406`
//! / `scan_rescan_same_part_idempotent_already_present` 等旧断言在新设计下已不再适用（DTO 字段
//! 删 `assembly_id` / `child_count` / `already_present`；语义由原子失败改为 5 类分组 + 软成功）。
//! 本 Task 7 新增的 7 个测试用新 DTO 形态重新覆盖：
//!
//! ## 并行 / 认证
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。
//! 每个用例 MANAGER token（M/C/I 三角色之一都能用，本系列用 MANAGER）。

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

async fn insert_part(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    assembly_id: Option<i64>,
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
         VALUES ($1, $2, $3, 'D-001', $4, 'INSPECTION', $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL, $7)",
        id, serial_no, name, customer_id, now, today, assembly_id,
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

/// Task 7：scan-route-b 5 类状态分组测试用 helper。
///
/// 与 `insert_batch` 的差异：
/// - 走 `t_part_batch_id_seq`（非雪花 ID），让测试断言不依赖雪花规则
/// - `version = 1`（与 `attach_to_note` 的乐观锁语义对齐：默认 0 也能过，但显式 1 表达更清晰）
/// - 必传 `current_holder_id` + `location`（B/C 组边界场景必须；工人持有 = `Some("WORKER")`）
/// - 不写 `created_at` / `updated_at` / `created_by` 等审计字段（DB DEFAULT 即可）
async fn create_test_batch(
    pool: &sqlx::PgPool,
    part_id: i64,
    status: &str,
    holder_id: Option<i64>,
    location: Option<&str>,
) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO t_part_batch (part_id, batch_no, quantity, status, current_holder_id, location, version) \
         VALUES ($1, 1, 10, $2, $3, $4, 1) RETURNING id",
    )
    .bind(part_id)
    .bind(status)
    .bind(holder_id)
    .bind(location)
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

/// Task 9 helper：与 `create_test_batch` 同形但显式接受 `batch_no`，
/// 用于「同一 part 上挂多个批次」的场景（`uq_t_part_batch_part_no` 唯一约束）。
async fn create_test_batch_at(
    pool: &sqlx::PgPool,
    part_id: i64,
    batch_no: i32,
    status: &str,
    holder_id: Option<i64>,
    location: Option<&str>,
) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO t_part_batch (part_id, batch_no, quantity, status, current_holder_id, location, version) \
         VALUES ($1, $2, 10, $3, $4, $5, 1) RETURNING id",
    )
    .bind(part_id)
    .bind(batch_no)
    .bind(status)
    .bind(holder_id)
    .bind(location)
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

async fn insert_assembly(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
    drawing_no: &str,
    name: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query!(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, serial_no, quantity, \
         unit_price, total_price, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, '', $4, $5, $5, 'ACTIVE', $6, 1, 0, 0, 0, $7, NULL, $7, NULL)",
        id, drawing_no, name, customer_id, today, serial_no, now,
    )
    .execute(pool)
    .await
    .expect("insert assembly");
    id
}

async fn insert_group(pool: &PgPool, l1_id: i64, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_delivery_group (id, customer_id, name, version, created_at, \
         created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 0, $4, NULL, $4, NULL)",
        id, l1_id, name, now,
    )
    .execute(pool)
    .await
    .expect("insert group");
    id
}

async fn insert_group_member(pool: &PgPool, group_id: i64, l2_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_delivery_group_member (id, group_id, customer_id, created_at, created_by) \
         VALUES ($1, $2, $3, $4, NULL)",
        id, group_id, l2_id, now,
    )
    .execute(pool)
    .await
    .expect("insert group member");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn scan_empty_code_returns_400_20104() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let _ = insert_l2(&pool, "二厂", l1).await;
    let (app, token, _) = login_manager(pool, "admin").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "   "})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "trim→空: {env}");
    assert_eq!(env["code"], 20104);
}

#[tokio::test]
async fn scan_unknown_code_returns_404_21417() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let _ = insert_part(&pool, "P", l2, Some("ABCD1234"), None).await;

    let (app, token, _) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "NOPE9999"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(env["code"], 21417, "full = {env}");
}

#[tokio::test]
async fn scan_single_part_inspection_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("P001"), None).await;
    let bid = insert_batch(&pool, pid, 1, 3, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "P001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "first scan: {env}");
    assert_eq!(env["data"]["outcome"], "ADDED");
    assert_eq!(env["data"]["resolved"]["kind"], "PART");
    assert_eq!(env["data"]["resolved"]["serial_no"], "P001");
    assert_eq!(env["data"]["note"]["status"], "DRAFT");
    assert_eq!(
        env["data"]["note"]["scope_label"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        "法拉电子"
    );
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        1,
        "added_batches should be 1"
    );
    assert_eq!(
        env["data"]["added_batches"][0]["batch_id"]
            .as_str()
            .unwrap(),
        bid.to_string()
    );
    assert_eq!(env["data"]["note"]["line_count"], 1);

    // batch 已经挂到本单
    let dn_id: Option<i64> = sqlx::query_scalar!(
        "SELECT delivery_note_id FROM t_part_batch WHERE id = $1",
        bid
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        dn_id.is_some(),
        "batch should have delivery_note_id after scan attach"
    );
}

#[tokio::test]
async fn scan_rescan_same_part_idempotent_already_present() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("P002"), None).await;
    let _ = insert_batch(&pool, pid, 1, 3, "INSPECTION").await;

    let (app, token, _) = login_manager(pool, "admin").await;
    // First scan — Added
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "P002"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let note_id = env1["data"]["note"]["id"].as_str().unwrap().to_string();

    // Rescan — AlreadyPresent, no new batch
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "P002"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "rescan: {env2}");
    assert_eq!(env2["data"]["outcome"], "ALREADY_PRESENT");
    assert_eq!(env2["data"]["note"]["id"].as_str().unwrap(), note_id);
    // `already_present` 字段在 Task 3 DTO 精简中已移除；outcome=ALREADY_PRESENT
    // 与 note.id 不变即足以证明幂等性。
    assert!(
        env2["data"].get("already_present").is_none(),
        "新 DTO 无 already_present 字段"
    );
}

#[tokio::test]
async fn scan_part_in_process_returns_400_21405() {
    // 注：测试名仍叫 `returns_400_21405`，但在新设计下 `IN_PROCESS` + 无 holder 归 B 组
    // （返回 200 + CANDIDATES_AVAILABLE），不是 400/21405；要触发 C 组短路需带 holder
    // 且 `location='WORKER'`（仅 holder 不再够，因为 holder 多态，可能是货架）。
    // 这里给批次加上 Some(99) holder + Some("WORKER") → C 组短路 → 400/21421。
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("P003"), None).await;
    let bid = create_test_batch(&pool, pid, "IN_PROCESS", Some(99), Some("WORKER")).await;

    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "P003"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "C 组短路: {env}");
    assert_eq!(
        env["code"], 21421,
        "worker-held IN_PROCESS 走 classify_invalid_state → 21421；got: {env}"
    );
    let msg = env["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("IN_PROCESS") && msg.contains(&bid.to_string()),
        "message 应含 IN_PROCESS + batch id：{msg}"
    );
    let _ = l1;
}

/// 回归：`IN_PROCESS` + `location='PRODUCTION_SHELF'` + `current_holder_id=Some(shelf_id)`
///（多态 holder 指货架）应归 B 组而非 C 组短路。
///
/// 真实事故：工件 F1000 躺生产货架 A1 上，holder 存的是货架 id，
/// 旧判据 `current_holder_id IS NOT NULL` 直接判为 C 组 → 21421；
/// 新判据需 `location='WORKER'` 才走 C 组短路，
/// `PRODUCTION_SHELF` 应继续走 B 组 candidates 列表。
#[tokio::test]
async fn scan_part_in_process_on_production_shelf_returns_candidates() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("PE0002"), None).await;
    let bid = create_test_batch(
        &pool,
        pid,
        "IN_PROCESS",
        Some(88),
        Some("PRODUCTION_SHELF"),
    )
    .await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "PE0002"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "IN_PROCESS + PRODUCTION_SHELF 应归 B 组: {env}"
    );
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["outcome"],
        "CANDIDATES_AVAILABLE",
        "B 组应返回 CANDIDATES_AVAILABLE"
    );
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        0,
        "B 组不挂单"
    );

    let unresolved = env["data"]["unresolved_targets"]
        .as_array()
        .expect("unresolved_targets 应为数组");
    assert_eq!(unresolved.len(), 1, "散件 + 仅 B 组 → 单元素");
    let target = &unresolved[0];
    let target_pid: i64 = target["part_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(target_pid, pid);
    assert_eq!(target["serial_no"], "PE0002");

    let avail = target["available_batches"].as_array().expect("available_batches");
    assert_eq!(avail.len(), 1);
    assert_eq!(
        avail[0]["batch_id"].as_str().unwrap(),
        bid.to_string(),
        "available_batches[0].batch_id 应等于 B 组批次"
    );
    assert_eq!(avail[0]["status"], "IN_PROCESS");
    assert_eq!(avail[0]["quantity"], 10);
    assert!(
        avail[0]["version"].is_i64(),
        "available_batches[0].version 必须存在且为整数，实际 = {}",
        avail[0]["version"]
    );

    // batch 不应挂单
    let dn_id: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dn_id.is_none(), "B 组批次不应挂单");

    // 草稿应已建立（line_count=0，本单未挂批次）
    assert_eq!(env["data"]["note"]["status"], "DRAFT");
    assert_eq!(env["data"]["note"]["line_count"], 0);
}

#[tokio::test]
async fn scan_part_on_other_active_note_returns_409_21406() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("P004"), None).await;
    let bid = insert_batch(&pool, pid, 1, 3, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;

    // 建草稿 → 推上 READY → submit（让 note 进入 SUBMITTED，batch 仍挂在上面）。
    // 注意：submit 要求所有批次 READY_TO_SHIP，这里手动 SQL 升到该状态再调 submit。
    sqlx::query!(
        "UPDATE t_part_batch SET status = 'READY_TO_SHIP' WHERE id = $1",
        bid
    )
    .execute(&pool)
    .await
    .unwrap();

    let (cs, cenv) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": l1.to_string(),
                "items": [{"batch_id": bid.to_string()}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(cs, StatusCode::OK, "create draft: {cenv}");
    let other_id = cenv["data"]["id"].as_str().unwrap().to_string();
    let other_no = cenv["data"]["delivery_note_no"].as_str().unwrap().to_string();

    let (sub, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{other_id}/submit"),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(sub, StatusCode::OK, "submit note: {sub}");

    // scan: 此时无 DRAFT（已 submit），新建一个 DRAFT，batch 挂在 SUBMITTED 上 → conflict
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "P004"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "scan conflict: {env}");
    assert_eq!(env["code"], 21406);
    let msg = env["message"].as_str().unwrap_or("");
    // 新设计下其它单号不在 message 里；以 code 前缀 [21406] + 409 即可证明冲突语义。
    // 保留 other_no 用于回归人读友好（被 _ 前缀抑制）。
    let _ = other_no;
    assert!(
        msg.starts_with("[21406]"),
        "message 应以 [21406] 开头：{msg}"
    );
}

#[tokio::test]
async fn scan_assembly_full_all_subparts_ready_added() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // 装配件 + 3 个子件，全部 READY_TO_SHIP
    let asm_id = insert_assembly(&pool, l2, "L1067", "ASM-067", "总成067").await;
    let mut sub_pids: Vec<i64> = Vec::new();
    for i in 0..3 {
        let p = insert_part(&pool, &format!("Sub{i}"), l2, Some(&format!("S{i:04}")), Some(asm_id)).await;
        let _ = insert_batch(&pool, p, 1, 2, "READY_TO_SHIP").await;
        sub_pids.push(p);
    }
    assert_eq!(sub_pids.len(), 3);

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "L1067"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan assembly: {env}");
    assert_eq!(env["data"]["outcome"], "ADDED");
    assert_eq!(env["data"]["resolved"]["kind"], "ASSEMBLY");
    assert_eq!(env["data"]["resolved"]["id"].as_str().unwrap(), asm_id.to_string());
    // ResolvedEntityDto 不再有 child_count 字段（Task 3 DTO 精简移除）
    assert!(
        env["data"]["resolved"].get("child_count").is_none(),
        "新 DTO 应无 child_count 字段"
    );
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        3,
        "all 3 sub-part batches should be added"
    );
    assert_eq!(env["data"]["note"]["line_count"], 3);

    // 每个 sub part 的所有批次都已挂单
    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM t_part_batch \
         WHERE part_id = ANY($1) AND delivery_note_id IS NOT NULL",
        &sub_pids,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn scan_assembly_atomic_reject_with_failures() {
    // 注：原断言 400/21418 + failures 列表是 pre-route-b 原子拒绝语义；新设计下：
    //   - 任一子件 C 组 → 整单 400/21421（C 组短路）
    //   - A 组子件本可挂单，但事务回滚 → DB 上仍未挂
    // 此处把 Z0001 标为 worker-held IN_PROCESS（C 组）触发短路，验证 Z0000/Z0002（A 组）
    // 也未挂单（整单回滚的间接证据）。
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let asm_id = insert_assembly(&pool, l2, "L2099", "ASM-099", "总成099").await;

    // 3 子件：Z0000/Z0002 → A 组 INSPECTION；Z0001 → C 组（worker-held IN_PROCESS）
    let mut all_pids: Vec<i64> = Vec::new();
    for i in 0..3 {
        let p = insert_part(
            &pool,
            &format!("Sub{i}"),
            l2,
            Some(&format!("Z{i:04}")),
            Some(asm_id),
        )
        .await;
        all_pids.push(p);
        if i == 1 {
            // Z0001：C 组（带 holder 的 IN_PROCESS）
            let _ = create_test_batch(&pool, p, "IN_PROCESS", Some(99), Some("WORKER")).await;
        } else {
            // Z0000 / Z0002：A 组 INSPECTION
            let _ = create_test_batch(&pool, p, "INSPECTION", None, None).await;
        }
    }

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "L2099"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "C 组短路 → 整单拒绝: {env}");
    assert_eq!(
        env["code"], 21421,
        "worker-held IN_PROCESS 触发 classify_invalid_state → 21421；got: {env}"
    );
    let msg = env["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("IN_PROCESS"),
        "message 应含 IN_PROCESS：{msg}"
    );

    // 整单回滚：no batch attached（包含 A 组的 Z0000/Z0002 也未挂单）
    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM t_part_batch \
         WHERE delivery_note_id IN (SELECT id FROM t_delivery_note WHERE customer_id = $1)",
        l1,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "整单应回滚：C 组短路 → A 组子件批次也不应挂单；got count={count}"
    );

    // 进一步断言：3 个子件的 batch.delivery_note_id 全部为 NULL（A 组 Z0000/Z0002 也未挂）
    let attached_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_batch \
         WHERE part_id = ANY($1) AND delivery_note_id IS NOT NULL",
    )
    .bind(&all_pids)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        attached_count, 0,
        "整单事务回滚后，3 个子件的批次均不应挂单；got attached_count={attached_count}"
    );
}

#[tokio::test]
async fn scan_assembly_rescan_idempotent_already_present() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let asm_id = insert_assembly(&pool, l2, "L3055", "ASM-055", "总成055").await;
    for i in 0..2 {
        let p = insert_part(&pool, &format!("Sub{i}"), l2, Some(&format!("Y{i:04}")), Some(asm_id))
            .await;
        let _ = insert_batch(&pool, p, 1, 1, "READY_TO_SHIP").await;
    }

    let (app, token, _) = login_manager(pool, "admin").await;

    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "L3055"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first: {env1}");
    let note_id = env1["data"]["note"]["id"].as_str().unwrap().to_string();

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "L3055"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "rescan: {env2}");
    assert_eq!(env2["data"]["outcome"], "ALREADY_PRESENT");
    assert_eq!(env2["data"]["note"]["id"].as_str().unwrap(), note_id);
}

#[tokio::test]
async fn scan_auto_routes_by_l2_to_distinct_groups() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_in = insert_l2(&pool, "二厂", l1).await;
    let l2_out = insert_l2(&pool, "五厂", l1).await;
    // 建组：{二厂}
    let gid = insert_group(&pool, l1, "二五六厂").await;
    insert_group_member(&pool, gid, l2_in).await;

    // 二厂 part → 分组单
    let p_in = insert_part(&pool, "P_in", l2_in, Some("R0001"), None).await;
    let _ = insert_batch(&pool, p_in, 1, 1, "INSPECTION").await;
    // 五厂 part → 单厂单
    let p_out = insert_part(&pool, "P_out", l2_out, Some("R0002"), None).await;
    let _ = insert_batch(&pool, p_out, 1, 1, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "R0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "in: {env1}");
    let note1_id = env1["data"]["note"]["id"].as_str().unwrap().to_string();
    let dgid: Option<i64> = sqlx::query_scalar!(
        "SELECT delivery_group_id FROM t_delivery_note WHERE id = $1",
        note1_id.parse::<i64>().unwrap(),
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(dgid.is_some(), "group-scoped note");
    assert_eq!(dgid.unwrap(), gid);

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "R0002"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "out: {env2}");
    let note2_id = env2["data"]["note"]["id"].as_str().unwrap().to_string();
    assert_ne!(note1_id, note2_id, "应分到不同 note");
    let lcid: Option<i64> = sqlx::query_scalar!(
        "SELECT leaf_customer_id FROM t_delivery_note WHERE id = $1",
        note2_id.parse::<i64>().unwrap(),
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lcid, Some(l2_out));
}

#[tokio::test]
async fn scan_no_groups_for_l1_collapses_to_one_l1wide_note() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_a = insert_l2(&pool, "二厂", l1).await;
    let l2_b = insert_l2(&pool, "五厂", l1).await;
    // 不建任何分组 → L1Wide
    let pa = insert_part(&pool, "Pa", l2_a, Some("Q0001"), None).await;
    let pb = insert_part(&pool, "Pb", l2_b, Some("Q0002"), None).await;
    let _ = insert_batch(&pool, pa, 1, 1, "INSPECTION").await;
    let _ = insert_batch(&pool, pb, 1, 1, "INSPECTION").await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "Q0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first: {env1}");
    let note_id = env1["data"]["note"]["id"].as_str().unwrap().to_string();

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "Q0002"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "second (different L2): {env2}");
    let note2_id = env2["data"]["note"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        note_id, note2_id,
        "无分组 L1 下两个 L2 累加到同一草稿（L1Wide）"
    );

    // 双列都是 NULL（L1Wide）
    let row = sqlx::query!(
        "SELECT delivery_group_id, leaf_customer_id FROM t_delivery_note WHERE id = $1",
        note_id.parse::<i64>().unwrap(),
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(row.delivery_group_id.is_none());
    assert!(row.leaf_customer_id.is_none());
    assert_eq!(env2["data"]["note"]["line_count"], 2);
}

/// 2026-08-22：草稿卡片 `recent_items` 形状 + 长度上限回归。
///
/// 场景：1 个 part 上建 10 个不同 batch_no 的批次（status=READY_TO_SHIP），
/// 扫该 part 的 serial_no 一次 → 全部 10 批一次性落到同一 DRAFT。
///
/// 断言：
/// - `note.line_count == 10`（所有 10 个批次都挂上了）
/// - `note.recent_items.len() == 8`（服务端 LIMIT 8 截断）
/// - 每条都有 batch_id / part_id / serial_no / drawing_no / name 字段
/// - `order_no` 是 JSON 字符串或 null（Option<String> 兼容）
/// - 第一条 `batch_id` 是 10 个批次中 id 最大的（ORDER BY id DESC）
#[tokio::test]
async fn test_scan_recent_items_caps_at_8_and_includes_required_fields() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // 1 个 part，带订单号（验证 order_no 字段透传）
    let pid = insert_part(&pool, "RecentItemPart", l2, Some("REC00001"), None).await;
    sqlx::query!(
        "UPDATE t_part SET order_no = $1 WHERE id = $2",
        "ORDER-RECENT",
        pid,
    )
    .execute(&pool)
    .await
    .expect("set part order_no");
    // 10 个不同 batch_no 的批次（status=READY_TO_SHIP 即可入单）
    //
    // 注：测试 helper `insert_batch` 内部每次都新建一个 `SnowflakeIdGenerator`，
    // 在 10 次连续 await 中如果落在同一毫秒，会撞 `t_part_batch_pkey`。
    // 这里直接走单条多行 INSERT + 一次性生成 10 个雪花 ID 来规避。
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let mut created_batch_ids: Vec<i64> = Vec::with_capacity(10);
    for _ in 0..10 {
        created_batch_ids.push(snowflake.next_id());
    }
    let max_batch_id = *created_batch_ids.iter().max().unwrap();
    sqlx::query!(
        r#"
        INSERT INTO t_part_batch
            (id, part_id, batch_no, quantity, status, has_been_repaired,
             version, created_at, created_by, updated_at, updated_by)
        VALUES
            ($1, $11, 1, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($2, $11, 2, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($3, $11, 3, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($4, $11, 4, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($5, $11, 5, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($6, $11, 6, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($7, $11, 7, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($8, $11, 8, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($9, $11, 9, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL),
            ($10, $11, 10, 1, 'READY_TO_SHIP', false, 0, $12, NULL, $12, NULL)
        "#,
        created_batch_ids[0],
        created_batch_ids[1],
        created_batch_ids[2],
        created_batch_ids[3],
        created_batch_ids[4],
        created_batch_ids[5],
        created_batch_ids[6],
        created_batch_ids[7],
        created_batch_ids[8],
        created_batch_ids[9],
        pid,
        now,
    )
    .execute(&pool)
    .await
    .expect("insert 10 batches");
    assert_eq!(created_batch_ids.len(), 10);

    let (app, token, _) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "REC00001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan: {env}");
    assert_eq!(env["data"]["outcome"], "ADDED");
    // 全部 10 个批次挂上了（line_count 与 added_batches 都应是 10）
    assert_eq!(env["data"]["note"]["line_count"], 10);
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        10
    );

    // ===== 断言 recent_items 形状 + 长度 =====
    let recent = env["data"]["note"]["recent_items"]
        .as_array()
        .expect("note.recent_items array");
    assert_eq!(
        recent.len(),
        8,
        "recent_items 应被 LIMIT 8 截断；got {} items",
        recent.len()
    );

    // 第一条应该是 id 最大的批次（ORDER BY b.id DESC LIMIT 8）
    let first_batch_id: i64 = recent[0]["batch_id"]
        .as_str()
        .unwrap()
        .parse()
        .expect("batch_id 是字符串雪花 ID");
    assert_eq!(
        first_batch_id, max_batch_id,
        "第一条 batch_id 应该是 10 个批次中 id 最大的"
    );

    // 校验每个 item 都有必需字段 + 字段类型合理
    for (idx, item) in recent.iter().enumerate() {
        // batch_id / part_id：字符串雪花 id
        let bid_str = item["batch_id"]
            .as_str()
            .unwrap_or_else(|| panic!("recent[{idx}].batch_id 应该是字符串"));
        let _: i64 = bid_str.parse().expect("batch_id 可解析为 i64");
        let pid_str = item["part_id"]
            .as_str()
            .unwrap_or_else(|| panic!("recent[{idx}].part_id 应该是字符串"));
        let parsed_pid: i64 = pid_str.parse().expect("part_id 可解析为 i64");
        assert_eq!(
            parsed_pid, pid,
            "recent[{idx}].part_id 应等于该 part 的 id"
        );

        // serial_no / drawing_no / name：字符串（serial_no 可 null —— t_part.serial_no 是 nullable）
        assert!(
            item["serial_no"].is_string() || item["serial_no"].is_null(),
            "recent[{idx}].serial_no 应为 string 或 null"
        );
        assert_eq!(
            item["serial_no"].as_str().unwrap_or(""),
            "REC00001",
            "recent[{idx}].serial_no 透传自 part.serial_no"
        );
        assert!(
            item["drawing_no"].is_string(),
            "recent[{idx}].drawing_no 应为 string"
        );
        assert!(
            item["name"].is_string(),
            "recent[{idx}].name 应为 string"
        );

        // order_no：JSON null 或 string 都接受（DB 列 nullable）
        let on = &item["order_no"];
        assert!(
            on.is_null() || on.is_string(),
            "recent[{idx}].order_no 应为 null 或 string；got {on}"
        );
        assert_eq!(
            on.as_str().unwrap_or(""),
            "ORDER-RECENT",
            "recent[{idx}].order_no 透传自 part.order_no"
        );
    }

    // 额外断言：连续两条 batch_id 严格递减（ORDER BY id DESC 语义）
    let ids: Vec<i64> = recent
        .iter()
        .map(|it| it["batch_id"].as_str().unwrap().parse().unwrap())
        .collect();
    for w in ids.windows(2) {
        assert!(
            w[0] > w[1],
            "recent_items 应按 batch_id DESC；got {} then {}",
            w[0],
            w[1]
        );
    }
}

// ===========================================================================
//  Task 7: scan-route-b 5 类状态分组 + 21421 + 幂等（7 场景）
// ===========================================================================
//
// 5 类状态分组语义（详见 `src/modules/delivery_note/service/scan.rs:36-64`）：
//   A 组（attachable）：INSPECTION + READY_TO_SHIP          → 直接挂单
//   B 组（inspectable）：PENDING / PROGRAMMING / REPAIRING / IN_PROCESS(无 holder)
//                                                              → 走 candidates 列表
//   C 组（短路报错）：DELIVERED / OUTSOURCE / COMPLETED / CANCELLED /
//                      IN_PROCESS(有 holder)                 → 21421 硬错误
//
// outcome 4 变体（详见 dto.rs:467-472）：
//   ADDED：               散件/装配件 + A 组覆盖所有 target + 本次挂载 ≥1
//   ALREADY_PRESENT：     A 组覆盖所有 target，但都已在本单（幂等）
//   CANDIDATES_AVAILABLE：散件 + 仅 B 组 → unresolved_targets 单元素
//   PARTIAL_ADDED：       装配件 + A+B 混合 → unresolved_targets 多元素

/// 场景 1：散件 + 1 个 READY_TO_SHIP 批次 → 200 ADDED。
///
/// A 组覆盖，散件挂单成功，`added_batches=1`。
#[tokio::test]
async fn scan_standalone_part_with_ready_batch_returns_added() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("RB0001"), None).await;
    let bid = create_test_batch(&pool, pid, "READY_TO_SHIP", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "RB0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan READY_TO_SHIP: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["outcome"], "ADDED");
    assert_eq!(env["data"]["resolved"]["kind"], "PART");
    assert_eq!(env["data"]["resolved"]["serial_no"], "RB0001");
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        1,
        "A 组应挂 1 个批次"
    );
    let added = &env["data"]["added_batches"][0];
    assert_eq!(
        added["batch_id"].as_str().unwrap(),
        bid.to_string(),
        "added_batches[0].batch_id 应等于刚创建的批次"
    );
    // AddedBatchDto 不含 status 字段（精简：4 字段 batch_id/part_id/serial_no/quantity）
    assert!(
        added.get("status").is_none(),
        "AddedBatchDto 无 status 字段；got {added}"
    );
    assert_eq!(
        env["data"]["unresolved_targets"],
        serde_json::Value::Null,
        "ADDED 场景下 unresolved_targets 应为 null"
    );
    assert_eq!(env["data"]["note"]["line_count"], 1);

    // 落库断言：batch.delivery_note_id 已挂到本单
    let dn_id: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dn_id.is_some(), "batch 应已挂单");
}

/// 场景 2：散件 + 1 个 INSPECTION 批次 → 200 ADDED。
///
/// INSPECTION 也是 A 组（设计 `is_attachable_state`），直接挂单。
#[tokio::test]
async fn scan_standalone_part_with_inspection_batch_returns_added() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("IB0001"), None).await;
    let _ = create_test_batch(&pool, pid, "INSPECTION", None, None).await;

    let (app, token, _) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "IB0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan INSPECTION: {env}");
    assert_eq!(env["data"]["outcome"], "ADDED");
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        1
    );
    let added = &env["data"]["added_batches"][0];
    assert!(
        added.get("status").is_none(),
        "AddedBatchDto 无 status 字段；got {added}"
    );
}

/// 场景 3：散件 + 1 个 PENDING 批次 → 200 CANDIDATES_AVAILABLE。
///
/// PENDING 是 B 组（设计 `is_inspectable_state`），不挂单，
/// 走 `unresolved_targets` 单元素路径。
#[tokio::test]
async fn scan_standalone_part_with_only_pending_returns_candidates() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("PE0001"), None).await;
    let bid = create_test_batch(&pool, pid, "PENDING", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "PE0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan PENDING: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["outcome"], "CANDIDATES_AVAILABLE");
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        0,
        "B 组不挂单"
    );
    let unresolved = env["data"]["unresolved_targets"]
        .as_array()
        .expect("unresolved_targets 应为数组");
    assert_eq!(unresolved.len(), 1, "散件 + 仅 B 组 → 单元素");
    let target = &unresolved[0];
    let target_pid: i64 = target["part_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(target_pid, pid);
    assert_eq!(target["serial_no"], "PE0001");

    let avail = target["available_batches"].as_array().expect("available_batches");
    assert_eq!(avail.len(), 1);
    assert_eq!(
        avail[0]["batch_id"].as_str().unwrap(),
        bid.to_string(),
        "available_batches[0].batch_id 应等于 B 组批次"
    );
    assert_eq!(avail[0]["status"], "PENDING");
    assert_eq!(avail[0]["quantity"], 10);
    assert!(
        avail[0]["version"].is_i64(),
        "available_batches[0].version 必须存在且为整数，实际 = {}",
        avail[0]["version"]
    );

    // batch 不应挂单
    let dn_id: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dn_id.is_none(), "B 组批次不应挂单");

    // 草稿应已建立（line_count=0，本单未挂批次）
    assert_eq!(env["data"]["note"]["status"], "DRAFT");
    assert_eq!(env["data"]["note"]["line_count"], 0);
}

/// 场景 4：装配件 + 子件全 A 组（READY_TO_SHIP）→ 200 ADDED。
///
/// `is_assembly=true`、`any_inspectable=false`、`all_attachable_empty=false`
/// → `classify_outcome` 返回 ADDED；全部子件挂入 added_batches。
#[tokio::test]
async fn scan_assembly_with_all_ready_returns_added() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let asm_id = insert_assembly(&pool, l2, "ASM-R1", "ASM-R1-DWG", "全A组装").await;
    let mut sub_pids: Vec<i64> = Vec::new();
    let mut sub_bids: Vec<i64> = Vec::new();
    for i in 0..3 {
        let p = insert_part(
            &pool,
            &format!("SubR{i}"),
            l2,
            Some(&format!("SR{i:04}")),
            Some(asm_id),
        )
        .await;
        let b = create_test_batch(&pool, p, "READY_TO_SHIP", None, None).await;
        sub_pids.push(p);
        sub_bids.push(b);
    }

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "ASM-R1"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan assembly all-A: {env}");
    assert_eq!(env["data"]["outcome"], "ADDED");
    assert_eq!(env["data"]["resolved"]["kind"], "ASSEMBLY");
    assert_eq!(
        env["data"]["resolved"]["id"].as_str().unwrap(),
        asm_id.to_string()
    );
    let added = env["data"]["added_batches"].as_array().unwrap();
    assert_eq!(added.len(), 3, "全 A 组 → 全部 3 个子件批次挂单");
    assert_eq!(env["data"]["note"]["line_count"], 3);
    // unresolved_targets 应为 null（Added 路径不构建 B 组列表）
    assert_eq!(
        env["data"]["unresolved_targets"],
        serde_json::Value::Null
    );

    // DB 层断言：3 个子件批次都已挂到该草稿的 delivery_note_id
    let dn_id_i64: i64 = env["data"]["note"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let attached: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_batch \
         WHERE part_id = ANY($1) AND delivery_note_id = $2",
    )
    .bind(&sub_pids)
    .bind(dn_id_i64)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attached, 3, "3 个子件批次应全部挂到本单");

    // ResolvedEntityDto 不再有 child_count 字段（DTO 精简）
    assert!(
        env["data"]["resolved"].get("child_count").is_none(),
        "新 DTO 应无 child_count 字段"
    );
    let _ = sub_bids;
}

/// 场景 5：装配件 + A+B 子件混合 → 200 PARTIAL_ADDED。
///
/// 3 子件：2 READY_TO_SHIP（A）+ 1 PENDING（B）。
/// `is_assembly=true`、`any_inspectable=true` → `classify_outcome`
/// 返回 PARTIAL_ADDED。
///
/// 2026-08-31 修订：A 组不再由 scan 自动 attach（即使在 PARTIAL_ADDED 下）；
/// 所有有 A 或 B 的子件都进 unresolved_targets，A 组放 attachable_batches，
/// B 组放 available_batches，由前端弹窗勾选后转发 `POST /{id}/attach-batches`。
#[tokio::test]
async fn scan_assembly_with_partial_ready_returns_partial_added() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let asm_id = insert_assembly(&pool, l2, "ASM-P1", "ASM-P1-DWG", "A+B组装").await;
    let mut a_pids: Vec<i64> = Vec::new();
    let mut b_pid: i64 = 0;
    let mut b_bid: i64 = 0;
    for i in 0..3 {
        let p = insert_part(
            &pool,
            &format!("SubP{i}"),
            l2,
            Some(&format!("SP{i:04}")),
            Some(asm_id),
        )
        .await;
        if i < 2 {
            // A 组
            create_test_batch(&pool, p, "READY_TO_SHIP", None, None).await;
            a_pids.push(p);
        } else {
            // B 组
            let b = create_test_batch(&pool, p, "PENDING", None, None).await;
            b_pid = p;
            b_bid = b;
        }
    }

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "ASM-P1"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan assembly A+B: {env}");
    assert_eq!(env["data"]["outcome"], "PARTIAL_ADDED");
    assert_eq!(env["data"]["resolved"]["kind"], "ASSEMBLY");

    // 关键：PARTIAL_ADDED 下 A 组不再挂单（added_batches 必须为空）
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        0,
        "PARTIAL_ADDED 下 A 组不再自动 attach（2026-08-31 修订）"
    );
    assert_eq!(
        env["data"]["note"]["line_count"], 0,
        "没有任何 batch 被自动 attach"
    );

    // 3 个有 A 或 B 的子件都进 unresolved_targets（A-only 也进，等前端弹窗勾选）
    let unresolved = env["data"]["unresolved_targets"]
        .as_array()
        .expect("unresolved_targets 应为数组");
    assert_eq!(
        unresolved.len(),
        3,
        "3 个有 A 或 B 的子件都进 unresolved_targets"
    );

    // 按 part_id 分类断言
    let mut by_pid: std::collections::HashMap<i64, &Value> =
        std::collections::HashMap::new();
    for t in unresolved {
        let pid: i64 = t["part_id"].as_str().unwrap().parse().unwrap();
        by_pid.insert(pid, t);
    }

    // A-only 子件：attachable_batches 非空，available_batches 空
    for a_pid in &a_pids {
        let t = by_pid
            .get(a_pid)
            .unwrap_or_else(|| panic!("A 子件 {a_pid} 应在 unresolved_targets"));
        let att = t["attachable_batches"]
            .as_array()
            .expect("attachable_batches 字段必须存在");
        assert_eq!(att.len(), 1, "A 子件 attachable_batches=1");
        assert_eq!(att[0]["status"], "READY_TO_SHIP");
        let av = t["available_batches"]
            .as_array()
            .expect("available_batches 字段必须存在");
        assert_eq!(av.len(), 0, "A 子件 available_batches 应为空");
    }

    // B 子件：attachable 空，available_batches 含 B
    let b_target = by_pid
        .get(&b_pid)
        .expect("B 子件应在 unresolved_targets");
    assert_eq!(b_target["serial_no"], "SP0002");
    let b_att = b_target["attachable_batches"]
        .as_array()
        .expect("attachable_batches 字段必须存在");
    assert_eq!(b_att.len(), 0, "B 子件 attachable_batches 应为空");
    let b_avail = b_target["available_batches"]
        .as_array()
        .expect("available_batches 字段必须存在");
    assert_eq!(b_avail.len(), 1);
    assert_eq!(
        b_avail[0]["batch_id"].as_str().unwrap(),
        b_bid.to_string()
    );
    assert_eq!(b_avail[0]["status"], "PENDING");
    assert!(
        b_avail[0]["version"].is_i64(),
        "available_batches[0].version 必须存在且为整数，实际 = {}",
        b_avail[0]["version"]
    );

    // B 组批次不进 delivery_note_id
    let b_dn_id: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(b_bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(b_dn_id.is_none(), "B 组批次不应挂单");

    // A 组 2 个批次也都未挂本单（PARTIAL_ADDED 不写）
    let dn_id_i64: i64 = env["data"]["note"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let a_attached: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_batch \
         WHERE part_id = ANY($1) AND delivery_note_id = $2",
    )
    .bind(&a_pids)
    .bind(dn_id_i64)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        a_attached, 0,
        "PARTIAL_ADDED 下 A 组批次也不挂单（弹窗勾选后才挂）"
    );

    // ResolvedEntityDto 字段（无 assembly_id / child_count）
    assert!(env["data"]["resolved"].get("assembly_id").is_none());
    assert!(env["data"]["resolved"].get("child_count").is_none());
}

/// 场景 6：任一批次 DELIVERED → 400，code=21421。
///
/// C 组短路（`classify_invalid_state`）：DELIVERED 直接报
/// `BIZ_DELIVERY_BATCH_STATE_INVALID`，不挂任何批次（整单回滚）。
#[tokio::test]
async fn scan_with_delivered_batch_returns_21421() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("DV0001"), None).await;
    let bid = create_test_batch(&pool, pid, "DELIVERED", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "DV0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "C 组短路应 4xx: {env}");
    assert_eq!(
        env["code"], 21421,
        "C 组 DELIVERED 应返 BIZ_DELIVERY_BATCH_STATE_INVALID；got: {env}"
    );
    // 错误信息应含 DELIVERED + batch id（classify_invalid_state 返回 reason 字符串）
    let msg = env["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("DELIVERED") && msg.contains(&bid.to_string()),
        "message 应含 'DELIVERED' 与批次 id：{msg}"
    );

    // 短路语义：DELIVERED 批次不应挂单
    let dn_id: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        dn_id.is_none(),
        "C 组短路 → batch 不应挂单；got dn_id={dn_id:?}"
    );
}

/// 场景 7：同 serial_no 连续两次扫码 → ADDED 后 ALREADY_PRESENT（幂等）。
///
/// 第一次：散件 + INSPECTION → ADDED（挂 1 批）。
/// 第二次：同 serial_no → outcome=ALREADY_PRESENT（不再挂），
/// `added_batches=[]`、`unresolved_targets=null`，且 note.id 不变。
#[tokio::test]
async fn scan_twice_same_code_is_idempotent() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("ID0001"), None).await;
    let _ = create_test_batch(&pool, pid, "INSPECTION", None, None).await;

    let (app, token, _) = login_manager(pool, "admin").await;

    // 第一次 → ADDED
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "ID0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first scan: {env1}");
    assert_eq!(env1["data"]["outcome"], "ADDED");
    assert_eq!(env1["data"]["added_batches"].as_array().unwrap().len(), 1);
    let note_id_first = env1["data"]["note"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(env1["data"]["note"]["line_count"], 1);

    // 第二次 → ALREADY_PRESENT（幂等）
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "ID0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "second scan: {env2}");
    assert_eq!(env2["data"]["outcome"], "ALREADY_PRESENT");
    assert_eq!(
        env2["data"]["added_batches"].as_array().unwrap().len(),
        0,
        "ALREADY_PRESENT 不再挂批次"
    );
    assert_eq!(
        env2["data"]["unresolved_targets"],
        serde_json::Value::Null,
        "ALREADY_PRESENT 场景无 unresolved_targets"
    );
    let note_id_second = env2["data"]["note"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        note_id_first, note_id_second,
        "ALREADY_PRESENT 应回到同一草稿"
    );
    assert_eq!(
        env2["data"]["note"]["line_count"], 1,
        "line_count 不变（仍为 1）"
    );

    // 新 DTO 不再有 `already_present` / `skipped` 字段（DTO 精简）
    assert!(
        env2["data"].get("already_present").is_none(),
        "新 DTO 无 already_present 字段"
    );
    assert!(
        env2["data"].get("skipped").is_none(),
        "新 DTO 无 skipped 字段"
    );
}

// ===========================================================================
//  Task 9: scan outcome 行为细化 — A 组不再自动 attach（除「全 A 无 B」外）
// ===========================================================================
//
// 2026-08-31 路线 B 修订后语义：
//   - 散件全 A 组（无 B）  → ADDED + 自动 attach（向后兼容；保持原行为）
//   - 散件 A+B 混合       → CANDIDATES_AVAILABLE + attachable=A, available=B
//                            不再自动 attach（即使 A 组齐全）
//   - 散件 A+C（任一 C）   → CANDIDATES_AVAILABLE + attachable=A, available=空
//                            （C 静默过滤，但只要原批次含 C 就不走 auto-attach 路径）
//   - 散件 B+C            → CANDIDATES_AVAILABLE + attachable=空, available=B
//   - 装配件 A+B 混合      → PARTIAL_ADDED，每个有 A 或 B 的子件进 unresolved_targets；
//                            A-only 子件也进 unresolved_targets，attachable_batches 含 A 组
//
// 关键点：所有 CANDIDATES_AVAILABLE / PARTIAL_ADDED 场景下 `added_batches` 必须为空，
// A 组只能通过前端弹窗勾选后转发到 `POST /{id}/attach-batches` 显式 attach。

/// Task 9 场景 1：散件 + 2 个 A 组（INSPECTION）→ outcome=ADDED。
///
/// 全 A 组（无 B 无 C）→ 自动 attach，`unresolved_targets` 必须为 null。
/// 这是「全 A 无 B」路径的回归测试，必须仍能向后兼容走 ADDED。
#[tokio::test]
async fn scan_standalone_full_a_returns_added_with_no_unresolved() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P_full_a", l2, Some("FULLA0001"), None).await;
    // 2 个 A 组批次（INSPECTION）→ batch_no 不同以满足 (part_id, batch_no) UNIQUE
    let b1 = create_test_batch_at(&pool, pid, 1, "INSPECTION", None, None).await;
    let b2 = create_test_batch_at(&pool, pid, 2, "INSPECTION", None, None).await;

    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "FULLA0001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "全 A 组应 ADDED: {env}");
    assert_eq!(env["data"]["outcome"], "ADDED");
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        2,
        "全 A 组 2 个批次都应挂单"
    );
    let added_ids: Vec<String> = env["data"]["added_batches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["batch_id"].as_str().unwrap().to_string())
        .collect();
    assert!(added_ids.contains(&b1.to_string()));
    assert!(added_ids.contains(&b2.to_string()));
    assert_eq!(
        env["data"]["unresolved_targets"],
        serde_json::Value::Null,
        "ADDED 场景下 unresolved_targets 必须为 null"
    );
    assert_eq!(env["data"]["note"]["line_count"], 2);
}

/// Task 9 场景 2：散件 + [A, B] 混合 → outcome=CANDIDATES_AVAILABLE，
/// A 放进 attachable_batches，B 放进 available_batches；**不再自动 attach**。
///
/// 这是路线 B 的核心变更点：即使 A 组齐全，也不再自动 attach，
/// 留给前端弹窗勾选决定（POST /{id}/attach-batches 显式提交）。
#[tokio::test]
async fn scan_standalone_a_plus_b_returns_candidates_with_attachable_and_available() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P_apb", l2, Some("APB00001"), None).await;
    let a_bid = create_test_batch_at(&pool, pid, 1, "INSPECTION", None, None).await;
    let b_bid = create_test_batch_at(&pool, pid, 2, "PENDING", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "APB00001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "A+B 散件: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["outcome"],
        "CANDIDATES_AVAILABLE",
        "A+B 混合必须走 CANDIDATES_AVAILABLE；当前实现若直接 ADDED 则是回归"
    );

    // 关键断言：A 不再自动 attach
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        0,
        "CANDIDATES_AVAILABLE 下 added_batches 必须为空（A 由前端弹窗决定是否 attach）"
    );

    // unresolved_targets 单元素，含 A 和 B
    let unresolved = env["data"]["unresolved_targets"]
        .as_array()
        .expect("unresolved_targets 应为数组");
    assert_eq!(unresolved.len(), 1, "散件 → 单元素 unresolved_target");
    let target = &unresolved[0];
    let target_pid: i64 = target["part_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(target_pid, pid);

    // A 组 → attachable_batches
    let attachable = target["attachable_batches"]
        .as_array()
        .expect("attachable_batches 字段必须存在");
    assert_eq!(attachable.len(), 1, "A 组 1 个批次应进 attachable_batches");
    assert_eq!(
        attachable[0]["batch_id"].as_str().unwrap(),
        a_bid.to_string()
    );
    assert_eq!(attachable[0]["status"], "INSPECTION");
    assert_eq!(attachable[0]["quantity"], 10);
    assert!(
        attachable[0]["version"].is_i64(),
        "attachable_batches[].version 必须存在；got {}",
        attachable[0]["version"]
    );

    // B 组 → available_batches
    let available = target["available_batches"]
        .as_array()
        .expect("available_batches 字段必须存在");
    assert_eq!(available.len(), 1, "B 组 1 个批次应进 available_batches");
    assert_eq!(
        available[0]["batch_id"].as_str().unwrap(),
        b_bid.to_string()
    );
    assert_eq!(available[0]["status"], "PENDING");
    assert!(
        available[0]["version"].is_i64(),
        "available_batches[].version 必须存在；got {}",
        available[0]["version"]
    );

    // DB 校验：A 和 B 都未挂单
    for bid in [a_bid, b_bid] {
        let dn_id: Option<i64> =
            sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
                .bind(bid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            dn_id.is_none(),
            "CANDIDATES_AVAILABLE 下 batch {bid} 不应挂单（前端还没勾选）"
        );
    }

    // 草稿已建立但 line_count=0（没有任何 batch 自动 attach）
    assert_eq!(env["data"]["note"]["status"], "DRAFT");
    assert_eq!(env["data"]["note"]["line_count"], 0);
}

/// Task 9 场景 3：散件 + [A, C@WORKER] → outcome=CANDIDATES_AVAILABLE，
/// attachable_batches 含 A，available_batches 空。
///
/// C 组（工人持有 IN_PROCESS）由 has_fully_invalid_target 静默过滤（不是「全 C」，
/// 因此不报 21421）；但只要原批次集合含 C，就不走 auto-attach 路径，
/// A 组放进 attachable_batches 供前端决定。
#[tokio::test]
async fn scan_standalone_a_plus_c_returns_candidates_with_only_attachable() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P_apc", l2, Some("APC00001"), None).await;
    let a_bid = create_test_batch_at(&pool, pid, 1, "READY_TO_SHIP", None, None).await;
    let c_bid = create_test_batch_at(&pool, pid, 2, "IN_PROCESS", Some(99), Some("WORKER")).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "APC00001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "A+C 散件: {env}");
    assert_eq!(
        env["code"], 0,
        "非全 C 不应触发 21421；当前实现已静默过滤 C：{env}"
    );
    assert_eq!(
        env["data"]["outcome"],
        "CANDIDATES_AVAILABLE",
        "含 C 的原批次集合必须走 CANDIDATES_AVAILABLE（不走 auto-attach）；\
         若当前实现是 ADDED 则是任务约定的偏差，需要 backend 调整"
    );

    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        0,
        "CANDIDATES_AVAILABLE 下 A 也不再自动 attach"
    );

    let unresolved = env["data"]["unresolved_targets"]
        .as_array()
        .expect("unresolved_targets 应为数组");
    assert_eq!(unresolved.len(), 1, "散件 → 单元素 unresolved_target");

    // C 过滤后只剩 A → attachable_batches 有 1 个
    let attachable = unresolved[0]["attachable_batches"]
        .as_array()
        .expect("attachable_batches 字段必须存在");
    assert_eq!(
        attachable.len(),
        1,
        "C 过滤后 A 组 1 个批次应进 attachable_batches"
    );
    assert_eq!(
        attachable[0]["batch_id"].as_str().unwrap(),
        a_bid.to_string()
    );
    assert_eq!(attachable[0]["status"], "READY_TO_SHIP");

    // available_batches 必须为空（没有 B 组）
    let available = unresolved[0]["available_batches"]
        .as_array()
        .expect("available_batches 字段必须存在");
    assert_eq!(
        available.len(),
        0,
        "available_batches 应为空（C 已过滤，无 B）"
    );

    // DB 校验：A 和 C 都未挂单
    for bid in [a_bid, c_bid] {
        let dn_id: Option<i64> =
            sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
                .bind(bid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            dn_id.is_none(),
            "batch {bid} 不应挂单；got dn_id={dn_id:?}"
        );
    }

    assert_eq!(env["data"]["note"]["status"], "DRAFT");
    assert_eq!(env["data"]["note"]["line_count"], 0);
}

/// Task 9 场景 4：散件 + [B, C@WORKER] → outcome=CANDIDATES_AVAILABLE，
/// attachable_batches 空，available_batches 含 B。
///
/// C 静默过滤，B 进 available 候选，前端送检流程按 B 组处理。
#[tokio::test]
async fn scan_standalone_b_plus_c_returns_candidates_with_only_available() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P_bpc", l2, Some("BPC00001"), None).await;
    let b_bid = create_test_batch_at(&pool, pid, 1, "PENDING", None, None).await;
    let c_bid = create_test_batch_at(&pool, pid, 2, "IN_PROCESS", Some(99), Some("WORKER")).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "BPC00001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "B+C 散件: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["outcome"], "CANDIDATES_AVAILABLE");

    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        0
    );

    let unresolved = env["data"]["unresolved_targets"]
        .as_array()
        .expect("unresolved_targets 应为数组");
    assert_eq!(unresolved.len(), 1);

    // 没有 A 组 → attachable_batches 为空
    let attachable = unresolved[0]["attachable_batches"]
        .as_array()
        .expect("attachable_batches 字段必须存在");
    assert_eq!(
        attachable.len(),
        0,
        "无 A 组 → attachable_batches 应为空"
    );

    // B 进 available_batches
    let available = unresolved[0]["available_batches"]
        .as_array()
        .expect("available_batches 字段必须存在");
    assert_eq!(available.len(), 1);
    assert_eq!(
        available[0]["batch_id"].as_str().unwrap(),
        b_bid.to_string()
    );
    assert_eq!(available[0]["status"], "PENDING");

    // DB 校验：B 和 C 都未挂单
    for bid in [b_bid, c_bid] {
        let dn_id: Option<i64> =
            sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
                .bind(bid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(dn_id.is_none(), "batch {bid} 不应挂单");
    }

    assert_eq!(env["data"]["note"]["status"], "DRAFT");
    assert_eq!(env["data"]["note"]["line_count"], 0);
}

/// Task 9 场景 5：装配件 1 子件 [A,A]，1 子件 [B,B] → PARTIAL_ADDED，
/// 两个子件都进 unresolved_targets，每个子件都同时有 attachable + available。
///
/// 与既有 `scan_assembly_with_partial_ready_returns_partial_added` 的差异：
/// 这里**不**走 auto-attach（PARTIAL_ADDED 下 added_batches 必须为空）；
/// 每个有 A 或 B 的子件都进 unresolved_targets（A-only 子件也进，
/// attachable_batches 含其 A 组，等待前端弹窗勾选）。
#[tokio::test]
async fn scan_assembly_child_a_plus_b_returns_partial_added_with_attachable_per_child() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let asm_id = insert_assembly(&pool, l2, "ASM-MAB", "ASM-MAB-DWG", "A+B双批子件").await;

    // 子件 1：[A,A]（READY_TO_SHIP 各 1 个）→ batch_no 不同
    let child1_pid = insert_part(
        &pool,
        "ChildMAB-A",
        l2,
        Some("CMA0001"),
        Some(asm_id),
    )
    .await;
    let c1_a1 = create_test_batch_at(&pool, child1_pid, 1, "READY_TO_SHIP", None, None).await;
    let c1_a2 = create_test_batch_at(&pool, child1_pid, 2, "READY_TO_SHIP", None, None).await;

    // 子件 2：[B,B]（PENDING 各 1 个）→ batch_no 不同
    let child2_pid = insert_part(
        &pool,
        "ChildMAB-B",
        l2,
        Some("CMB0001"),
        Some(asm_id),
    )
    .await;
    let c2_b1 = create_test_batch_at(&pool, child2_pid, 1, "PENDING", None, None).await;
    let c2_b2 = create_test_batch_at(&pool, child2_pid, 2, "PENDING", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "ASM-MAB"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "装配件 A+B 双批子件: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["outcome"], "PARTIAL_ADDED");
    assert_eq!(env["data"]["resolved"]["kind"], "ASSEMBLY");

    // 关键断言：A 不再自动 attach
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        0,
        "PARTIAL_ADDED 下 A 组不再自动 attach；走前端弹窗"
    );

    // 两个子件都进 unresolved_targets
    let unresolved = env["data"]["unresolved_targets"]
        .as_array()
        .expect("unresolved_targets 应为数组");
    assert_eq!(
        unresolved.len(),
        2,
        "两个子件都进 unresolved_targets（A-only 子件也保留以供前端弹窗）"
    );

    // 按 part_id 排序后断言每个子件的 attachable / available
    let mut by_pid: std::collections::HashMap<i64, &Value> =
        std::collections::HashMap::new();
    for t in unresolved {
        let pid: i64 = t["part_id"].as_str().unwrap().parse().unwrap();
        by_pid.insert(pid, t);
    }

    // 子件 1 [A,A]：attachable=2, available=0
    let t1 = by_pid
        .get(&child1_pid)
        .expect("子件 1 应在 unresolved_targets 中");
    assert_eq!(t1["serial_no"], "CMA0001");
    let a1 = t1["attachable_batches"]
        .as_array()
        .expect("子件 1 attachable_batches");
    assert_eq!(a1.len(), 2, "子件 1 全部 A 组 → attachable_batches=2");
    let a1_ids: Vec<String> = a1
        .iter()
        .map(|b| b["batch_id"].as_str().unwrap().to_string())
        .collect();
    assert!(a1_ids.contains(&c1_a1.to_string()));
    assert!(a1_ids.contains(&c1_a2.to_string()));
    for b in a1 {
        assert_eq!(b["status"], "READY_TO_SHIP");
        assert!(b["version"].is_i64(), "version 字段必须存在");
    }
    let v1 = t1["available_batches"]
        .as_array()
        .expect("子件 1 available_batches");
    assert_eq!(v1.len(), 0, "子件 1 无 B 组 → available_batches=空");

    // 子件 2 [B,B]：attachable=0, available=2
    let t2 = by_pid
        .get(&child2_pid)
        .expect("子件 2 应在 unresolved_targets 中");
    assert_eq!(t2["serial_no"], "CMB0001");
    let a2 = t2["attachable_batches"]
        .as_array()
        .expect("子件 2 attachable_batches");
    assert_eq!(a2.len(), 0, "子件 2 无 A 组 → attachable_batches=空");
    let v2 = t2["available_batches"]
        .as_array()
        .expect("子件 2 available_batches");
    assert_eq!(v2.len(), 2, "子件 2 全部 B 组 → available_batches=2");
    let v2_ids: Vec<String> = v2
        .iter()
        .map(|b| b["batch_id"].as_str().unwrap().to_string())
        .collect();
    assert!(v2_ids.contains(&c2_b1.to_string()));
    assert!(v2_ids.contains(&c2_b2.to_string()));
    for b in v2 {
        assert_eq!(b["status"], "PENDING");
        assert!(b["version"].is_i64(), "version 字段必须存在");
    }

    // DB 校验：所有 4 个批次都未挂单（PARTIAL_ADDED 不写 DB）
    for bid in [c1_a1, c1_a2, c2_b1, c2_b2] {
        let dn_id: Option<i64> =
            sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
                .bind(bid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            dn_id.is_none(),
            "PARTIAL_ADDED 下 batch {bid} 不应挂单；got dn_id={dn_id:?}"
        );
    }

    assert_eq!(env["data"]["note"]["status"], "DRAFT");
    assert_eq!(env["data"]["note"]["line_count"], 0);
}

/// 2026-08-31：装配件 per-child had_invalid 不对称——1 子件原始含 C（C 过滤），
/// 另 1 子件纯 A。两者都应进 unresolved_targets，每个子件都正确携带 attachable_batches。
///
/// 这是 Task 4+7 review 发现的回归保护缺口（不对称 had_invalid 拓扑）：
/// child_a = [A, A, C@WORKER]（C 静默过滤后剩 2 个 A）；
/// child_b = [A]（纯 A）。
/// 两个子件都应进 unresolved_targets，且各自 attachable_batches 独立携带正确批次。
#[tokio::test]
async fn scan_assembly_asymmetric_had_invalid_per_child_returns_partial_added() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let asm_id = insert_assembly(
        &pool,
        l2,
        "ASM-ASYM",
        "ASM-ASYM-DWG",
        "A+C@WORKER vs A 不对称",
    )
    .await;

    // 子件 a：[A, A, C@WORKER] → C 在 had_invalid 路径被静默过滤
    let child_a = insert_part(&pool, "ChildAsym-A", l2, Some("CAA0001"), Some(asm_id)).await;
    let a1 = create_test_batch_at(&pool, child_a, 1, "INSPECTION", None, None).await;
    let a2 = create_test_batch_at(&pool, child_a, 2, "INSPECTION", None, None).await;
    let c1 =
        create_test_batch_at(&pool, child_a, 3, "IN_PROCESS", Some(99), Some("WORKER")).await;

    // 子件 b：纯 [A]
    let child_b = insert_part(&pool, "ChildAsym-B", l2, Some("CAB0001"), Some(asm_id)).await;
    let a3 = create_test_batch_at(&pool, child_b, 1, "INSPECTION", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "ASM-ASYM"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "装配件 A+C(不对称) + A: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["outcome"],
        "PARTIAL_ADDED",
        "装配+非纯A 或 任一子件含 C → PARTIAL_ADDED；got: {env}"
    );
    assert_eq!(env["data"]["resolved"]["kind"], "ASSEMBLY");

    // 关键断言：PARTIAL_ADDED 下 A 组不再自动 attach
    assert_eq!(
        env["data"]["added_batches"].as_array().unwrap().len(),
        0,
        "PARTIAL_ADDED 下 A 组不自动 attach"
    );

    // 两个子件都进 unresolved_targets（C 过滤对每个子件独立）
    let unresolved = env["data"]["unresolved_targets"]
        .as_array()
        .expect("unresolved_targets 应为数组");
    assert_eq!(
        unresolved.len(),
        2,
        "两个子件都进 unresolved_targets（child_a 含 C 但 had_invalid 后仍保留）"
    );

    // 按 part_id 分类断言每个子件的 attachable / available
    let mut by_pid: std::collections::HashMap<i64, &Value> =
        std::collections::HashMap::new();
    for t in unresolved {
        let pid: i64 = t["part_id"].as_str().unwrap().parse().unwrap();
        by_pid.insert(pid, t);
    }

    // child_a：C 过滤后剩 a1+a2 两个 A → attachable_batches=2，available=空
    let t_a = by_pid
        .get(&child_a)
        .expect("child_a 应在 unresolved_targets 中");
    assert_eq!(t_a["serial_no"], "CAA0001");
    let att_a = t_a["attachable_batches"]
        .as_array()
        .expect("child_a attachable_batches");
    assert_eq!(
        att_a.len(),
        2,
        "child_a C 过滤后剩 2 个 A → attachable_batches=2"
    );
    let att_a_ids: Vec<String> = att_a
        .iter()
        .map(|b| b["batch_id"].as_str().unwrap().to_string())
        .collect();
    assert!(att_a_ids.contains(&a1.to_string()));
    assert!(att_a_ids.contains(&a2.to_string()));
    for b in att_a {
        assert_eq!(b["status"], "INSPECTION");
        assert!(b["version"].is_i64(), "version 字段必须存在");
    }
    let av_a = t_a["available_batches"]
        .as_array()
        .expect("child_a available_batches");
    assert_eq!(av_a.len(), 0, "child_a 无 B 组 → available_batches=空");

    // child_b：纯 A 单 a3 → attachable_batches=1，available=空
    let t_b = by_pid
        .get(&child_b)
        .expect("child_b 应在 unresolved_targets 中");
    assert_eq!(t_b["serial_no"], "CAB0001");
    let att_b = t_b["attachable_batches"]
        .as_array()
        .expect("child_b attachable_batches");
    assert_eq!(att_b.len(), 1, "child_b 纯 A → attachable_batches=1");
    assert_eq!(att_b[0]["batch_id"].as_str().unwrap(), a3.to_string());
    assert_eq!(att_b[0]["status"], "INSPECTION");
    assert!(att_b[0]["version"].is_i64(), "version 字段必须存在");
    let av_b = t_b["available_batches"]
        .as_array()
        .expect("child_b available_batches");
    assert_eq!(av_b.len(), 0, "child_b 无 B 组 → available_batches=空");

    // DB 校验：c1 状态保持 IN_PROCESS（C 在 had_invalid 路径不动 DB）；
    // a1/a2/a3 都不挂单（PARTIAL_ADDED 不写 attachment）
    let c_status: String =
        sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
            .bind(c1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        c_status, "IN_PROCESS",
        "C 批次 c1 在 had_invalid 路径下状态不变"
    );

    for bid in [a1, a2, a3] {
        let dn_id: Option<i64> =
            sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
                .bind(bid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            dn_id.is_none(),
            "PARTIAL_ADDED 下 batch {bid} 不应挂单；got dn_id={dn_id:?}"
        );
    }

    assert_eq!(env["data"]["note"]["status"], "DRAFT");
    assert_eq!(env["data"]["note"]["line_count"], 0);
}
