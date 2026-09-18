//! part 域 Phase 1（2026-09-13）生命周期集成测试：1.1/1.2/1.3/1.4/1.7 端点。
//!
//! 覆盖：
//!   - place-on-shelf: PENDING → IN_PROCESS（happy + RBAC + 状态机拒绝 + shelf↔process 校验）
//!   - recall-to-pending: ON_SHELF/PROGRAMMING → PENDING（happy + 状态机拒绝）
//!   - send-to-programming: PENDING → PROGRAMMING
//!   - release-from-programming: PROGRAMMING → IN_PROCESS
//!   - recall-to-programming: ON_SHELF → PROGRAMMING
//!   - send-to-outsource: PENDING → OUTSOURCE
//!   - receive-from-outsource: OUTSOURCE → IN_PROCESS
//!   - receive-from-outsource-to-inspection: OUTSOURCE → INSPECTION
//!   - complete-repair: REPAIRING → IN_PROCESS / INSPECTION
//!   - repair-dispatch: 一步式返修下发
//!   - scan-inspect: 一步式扫码品检（PASS/FAIL）
//!   - scan-deliver-part: 司机扫码发货
//!
//! ## 并行 / 认证
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。

#[path = "common/mod.rs"]
mod common;

#[path = "part_api_helpers.rs"]
mod helpers;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    create_chain_for_part, create_step,add_role, clean_business_db, clean_db, insert_user_with_password, link_shelf_to_process, seed_process, test_app, test_pool, test_state};

use helpers::*;

// ===========================================================================
//  全局串行化
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
    common::ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

/// 创建一个 INSPECTOR 用户 + 登录拿 token。
async fn login_inspector(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "INSPECTOR", None, None).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(json!({"username": username, "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token, pool)
}

// ===========================================================================
//  1.1 place-on-shelf 测试
// ===========================================================================

#[tokio::test]
async fn place_on_shelf_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let prod_shelf = common::insert_shelf(&pool, "P-001", "生产架A", "PRODUCTION").await;
    let proc_id = seed_process(&pool, "PROC-A", "工序A").await;
    // PR-3: 建链并绑 part
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    link_shelf_to_process(&pool, prod_shelf, proc_id).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": prod_shelf.to_string(),
        "next_process_id": proc_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/place-on-shelf"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "place-on-shelf: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "IN_PROCESS");
}

#[tokio::test]
async fn place_on_shelf_rbac_clerk_ok() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let prod_shelf = common::insert_shelf(&pool, "P-002", "生产架B", "PRODUCTION").await;
    let proc_id = seed_process(&pool, "PROC-C", "工序C").await;
    // PR-3: 建链并绑 part
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    link_shelf_to_process(&pool, prod_shelf, proc_id).await;
    let (app, token, _pool) = login_clerk(pool, "clerk1").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": prod_shelf.to_string(),
        "next_process_id": proc_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/place-on-shelf"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "clerk should be allowed: {env}");
    assert_eq!(env["code"], 0);
}

#[tokio::test]
async fn place_on_shelf_invalid_transition_rejects() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // 工单 COMPLETED 状态（place-on-shelf 要求 PENDING）
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "COMPLETED").await;
    let bid = insert_batch(&pool, pid, 1, 5, "COMPLETED").await;
    let version = batch_version(&pool, bid).await;
    let prod_shelf = common::insert_shelf(&pool, "P-003", "生产架C", "PRODUCTION").await;
    let proc_id = seed_process(&pool, "PROC-D", "工序D").await;
    // PR-3: 建链并绑 part
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    link_shelf_to_process(&pool, prod_shelf, proc_id).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": prod_shelf.to_string(),
        "next_process_id": proc_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/place-on-shelf"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "COMPLETED → IN_PROCESS 应被状态机拒绝: {env}");
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn place_on_shelf_shelf_process_not_mapped_rejects() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let prod_shelf = common::insert_shelf(&pool, "P-004", "生产架D", "PRODUCTION").await;
    // 不创建映射 → 20507 BIZ_SHELF_PROCESS_NOT_MAPPED
    let proc_id = seed_process(&pool, "PROC-E", "工序E").await;
    // PR-3: 建链并绑 part
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": prod_shelf.to_string(),
        "next_process_id": proc_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/place-on-shelf"), Some(body), Some(&token)),
    )
    .await;
    // 20507 BIZ_SHELF_PROCESS_NOT_MAPPED → Phase 2 (2026-09-13) 显式映射 422
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "shelf↔process 缺失应拒绝: {env}");
    assert_eq!(env["code"], 20507);
}

#[tokio::test]
async fn recall_to_pending_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "IN_PROCESS").await;
    let bid = insert_batch(&pool, pid, 1, 5, "IN_PROCESS").await;
    // 写入 location=PRODUCTION_SHELF（recall-to-pending 要求）
    sqlx::query("UPDATE t_part_batch SET location = 'PRODUCTION_SHELF' WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .expect("set location");
    let version = batch_version(&pool, bid).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/recall-to-pending"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "recall: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "PENDING");
}

// ===========================================================================
//  1.2 CNC 编程流转测试
// ===========================================================================

#[tokio::test]
async fn send_to_programming_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/send-to-programming"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "send-to-programming: {env}");
    assert_eq!(env["data"]["status"], "PROGRAMMING");
}

#[tokio::test]
async fn release_from_programming_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PROGRAMMING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PROGRAMMING").await;
    let version = batch_version(&pool, bid).await;
    let prod_shelf = common::insert_shelf(&pool, "P-005", "生产架E", "PRODUCTION").await;
    let proc_id = seed_process(&pool, "PROC-F", "工序F").await;
    // PR-3: 建链并绑 part
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    link_shelf_to_process(&pool, prod_shelf, proc_id).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": prod_shelf.to_string(),
        "next_process_id": proc_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/release-from-programming"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "release: {env}");
    assert_eq!(env["data"]["status"], "IN_PROCESS");
}

#[tokio::test]
async fn release_from_programming_rbac_inspector_rejects() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PROGRAMMING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PROGRAMMING").await;
    let version = batch_version(&pool, bid).await;
    let prod_shelf = common::insert_shelf(&pool, "P-006", "生产架F", "PRODUCTION").await;
    let proc_id = seed_process(&pool, "PROC-G", "工序G").await;
    // PR-3: 建链并绑 part
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    link_shelf_to_process(&pool, prod_shelf, proc_id).await;
    let (app, token, _pool) = login_inspector(pool, "insp1").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": prod_shelf.to_string(),
        "next_process_id": proc_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/release-from-programming"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "INSPECTOR 不应能 release-from-programming: {env}");
    assert_eq!(env["code"], 40300);
}

// ===========================================================================
//  1.7 扫码检 / 司机扫码
// ===========================================================================

#[tokio::test]
async fn scan_inspect_pass_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let insp_shelf = common::insert_shelf(&pool, "I-001", "品检架A", "INSPECTION").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "pass": true,
        "target_inspection_shelf_id": insp_shelf.to_string(),
        "batch_id": bid.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/scan-inspect"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan-inspect PASS: {env}");
    assert_eq!(env["data"]["status"], "READY_TO_SHIP");
}

#[tokio::test]
async fn scan_inspect_fail_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "IN_PROCESS").await;
    let bid = insert_batch(&pool, pid, 1, 5, "IN_PROCESS").await;
    let version = batch_version(&pool, bid).await;
    let insp_shelf = common::insert_shelf(&pool, "I-002", "品检架B", "INSPECTION").await;
    // 添加短暂 sleep 避免雪花 ID 复用导致 shelf_id == process_id
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let prod_shelf = common::insert_shelf(&pool, "P-007", "生产架G", "PRODUCTION").await;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let proc_id = seed_process(&pool, "PROC-H", "工序H").await;
    link_shelf_to_process(&pool, prod_shelf, proc_id).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "pass": false,
        "target_inspection_shelf_id": insp_shelf.to_string(),
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": prod_shelf.to_string(),
        "next_process_id": proc_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/scan-inspect"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan-inspect FAIL: {env}");
    assert_eq!(env["data"]["status"], "REPAIRING");
}

#[tokio::test]
async fn scan_inspect_invalid_transition_rejects() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "DELIVERED").await;
    let bid = insert_batch(&pool, pid, 1, 5, "DELIVERED").await;
    let version = batch_version(&pool, bid).await;
    let insp_shelf = common::insert_shelf(&pool, "I-003", "品检架C", "INSPECTION").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "pass": true,
        "target_inspection_shelf_id": insp_shelf.to_string(),
        "batch_id": bid.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request("POST", &format!("/parts/{pid}/scan-inspect"), Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "DELIVERED 起点不允许: {env}");
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn scan_deliver_part_requires_driver() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // 创建 part 带 serial_no
    let pid = insert_part_with_status(&pool, "P0", l2, Some("B001-001"), None, "READY_TO_SHIP").await;
    let bid = insert_batch(&pool, pid, 1, 5, "READY_TO_SHIP").await;
    let _version = batch_version(&pool, bid).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    // 不创建任何 worker → 找不到工牌 → 401/404 错
    let body = json!({
        "part_serial_no": "B001-001",
        "worker_badge_code": "BADGE-001",
    });
    let (s, env) = send(
        app,
        json_request("POST", "/parts/scan/deliver-part", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "无效工牌应拒绝: {env}");
    assert_eq!(env["code"], 20201);
}
