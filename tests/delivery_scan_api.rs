//! delivery-notes/scan 扫码入单端到端集成测试
//!
//! Phase P3 覆盖（设计 §5 / §10）：
//!   1. 散件首次扫码 → 200 ADDED + added_batches=1 + line_count=1
//!   2. 散件同码重复扫码 → 200 ALREADY_PRESENT（幂等）
//!   3. 未知码 → 404 / 21417
//!   4. 散件 status=IN_PROCESS → 400 / 21405
//!   5. 散件已挂在其它 active 单 → 409 / 21406（note no 在 message）
//!   6. 装配件整套所有子件 READY → 200 ADDED + N 子件一批
//!   7. 装配件整套拒绝：1 子件 IN_PROCESS → 400 / 21418 + failures 含该子件
//!   8. 装配件整套重扫 → 200 ALREADY_PRESENT
//!   9. 同 L1 下跨 L2 自动分类：L2_in_group → 分组单，L2_out_group → 单厂单
//!  10. 无分组 L1：跨 L2 累加到同一草稿（L1Wide）
//!
//! 加场景（轻量）：scan 空码 → 400 / 20104。
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
    assert_eq!(env2["data"]["added_batches"].as_array().unwrap().len(), 0);
    assert_eq!(
        env2["data"]["already_present"].as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn scan_part_in_process_returns_400_21405() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P1", l2, Some("P003"), None).await;
    let _ = insert_batch(&pool, pid, 1, 1, "IN_PROCESS").await;

    let (app, token, _) = login_manager(pool, "admin").await;
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
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(env["code"], 21405);
    let msg = env["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("IN_PROCESS") || msg.contains("status"),
        "message 应含 status: {msg}"
    );
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
    assert!(
        msg.contains(&other_no),
        "message 应含其它单号 {other_no}, got: {msg}"
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
    assert_eq!(env["data"]["resolved"]["child_count"], 3);
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let asm_id = insert_assembly(&pool, l2, "L2099", "ASM-099", "总成099").await;
    // 3 子件：2 个 INSPECTION + 1 个 IN_PROCESS
    for i in 0..3 {
        let p = insert_part(&pool, &format!("Sub{i}"), l2, Some(&format!("Z{i:04}")), Some(asm_id))
            .await;
        let st = if i == 1 { "IN_PROCESS" } else { "INSPECTION" };
        let _ = insert_batch(&pool, p, 1, 1, st).await;
        let _ = p;
    }

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes/scan",
            Some(json!({"code": "L2099"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "atomic reject: {env}");
    assert_eq!(env["code"], 21418);
    // failures 应含 Z0001（IN_PROCESS）
    let failures = env["data"]["failures"].as_array().expect("data.failures");
    let any_bad = failures
        .iter()
        .any(|f| f["serial_no"] == "Z0001" && f["reason"].as_str().unwrap_or("").contains("IN_PROCESS"));
    assert!(any_bad, "failures 应含 Z0001+IN_PROCESS; got: {failures:?}");

    // ScanFailureDto 4 字段断言：part_id / batch_id / drawing_no / status
    // (Task 4 扩字段；Task 6 跟进端到端契约验证)
    let bad = failures
        .iter()
        .find(|f| f["serial_no"] == "Z0001")
        .expect("Z0001 in failures");
    // part_id 走 serialize_i64 → JSON string
    assert!(
        bad["part_id"].is_string(),
        "part_id 应为 string: {bad}"
    );
    let bad_pid: i64 = bad["part_id"]
        .as_str()
        .expect("part_id string")
        .parse()
        .expect("parse part_id");
    assert!(bad_pid > 0, "part_id 应 > 0; got {bad_pid}");
    // batch_id 与 drawing_no / status 在 not-ready 分支填全
    assert!(
        bad["batch_id"].is_string(),
        "batch_id 应为 string: {bad}"
    );
    let bad_bid: i64 = bad["batch_id"]
        .as_str()
        .expect("batch_id string")
        .parse()
        .expect("parse batch_id");
    assert!(bad_bid > 0, "batch_id 应 > 0; got {bad_bid}");
    assert_eq!(bad["drawing_no"], "D-001");
    assert_eq!(bad["status"], "IN_PROCESS");

    // 整单回滚：no batch attached
    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM t_part_batch \
         WHERE delivery_note_id IN (SELECT id FROM t_delivery_note WHERE customer_id = $1)",
        l1,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0, "整单应回滚：no batch attached");
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
