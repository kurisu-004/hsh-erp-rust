//! part 域 Phase 1（2026-09-13）返修闭环集成测试：1.4 端点。
//!
//! 覆盖：
//!   - complete_repair: REPAIRING → IN_PROCESS（PRODUCTION 区）
//!   - complete_repair: REPAIRING → INSPECTION（INSPECTION 区）
//!   - repair_dispatch: 一步式返修下发
//!   - list_repair_batches / list_repairing_batches

#[path = "common/mod.rs"]
mod common;

#[path = "part_api_helpers.rs"]
mod helpers;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    clean_business_db, clean_db, create_chain_for_part, create_step, link_shelf_to_process,
    seed_process, test_pool,
};

use helpers::*;

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

fn json_request(
    method: &str,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> Request<Body> {
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

#[tokio::test]
async fn complete_repair_to_process_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "REPAIRING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "REPAIRING").await;
    let version = batch_version(&pool, bid).await;
    let prod_shelf = common::insert_shelf(&pool, "P-R01", "生产架R1", "PRODUCTION").await;
    let proc_id = seed_process(&pool, "PROC-R1", "工序R1").await;

    // 2026-09-16 PR-3 批次 step 化：complete-repair / repair-dispatch
    // PRODUCTION 区要求 part 已绑定工艺链
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
        json_request(
            "POST",
            &format!("/parts/{pid}/complete-repair"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete-repair: {env}");
    assert_eq!(env["data"]["status"], "IN_PROCESS");
}

#[tokio::test]
async fn complete_repair_to_inspection_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "REPAIRING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "REPAIRING").await;
    let version = batch_version(&pool, bid).await;
    let insp_shelf = common::insert_shelf(&pool, "I-R01", "品检架R1", "INSPECTION").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": insp_shelf.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/complete-repair"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete-repair INSPECTION: {env}");
    assert_eq!(env["data"]["status"], "INSPECTION");
}

#[tokio::test]
async fn complete_repair_invalid_source_rejects() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let prod_shelf = common::insert_shelf(&pool, "P-R02", "生产架R2", "PRODUCTION").await;
    let proc_id = seed_process(&pool, "PROC-R2", "工序R2").await;
    link_shelf_to_process(&pool, prod_shelf, proc_id).await;
    // PR-3：repair-dispatch PRODUCTION 区要求 part 已绑定工艺链
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
        json_request(
            "POST",
            &format!("/parts/{pid}/complete-repair"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "PENDING 起点不允许: {env}");
    assert_eq!(env["code"], 20118);
}

#[tokio::test]
async fn repair_dispatch_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // 入口：INSPECTION / READY_TO_SHIP / IN_PROCESS / DELIVERED
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "INSPECTION").await;
    let bid = insert_batch(&pool, pid, 1, 5, "INSPECTION").await;
    let version = batch_version(&pool, bid).await;
    let insp_shelf = common::insert_shelf(&pool, "I-R03", "品检架R3", "INSPECTION").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": insp_shelf.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/repair-dispatch"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "repair-dispatch: {env}");
    assert_eq!(env["data"]["status"], "INSPECTION");
}

#[tokio::test]
async fn repair_dispatch_invalid_source_rejects() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "CANCELLED").await;
    let bid = insert_batch(&pool, pid, 1, 5, "CANCELLED").await;
    let version = batch_version(&pool, bid).await;
    let insp_shelf = common::insert_shelf(&pool, "I-R04", "品检架R4", "INSPECTION").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": insp_shelf.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/repair-dispatch"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "CANCELLED 起点不允许: {env}");
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn list_repair_batches_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "DELIVERED").await;
    insert_batch(&pool, pid, 1, 5, "DELIVERED").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts/repair-batches", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repair-batches: {env}");
    assert_eq!(env["code"], 0);
}

#[tokio::test]
async fn list_repairing_batches_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "REPAIRING").await;
    insert_batch(&pool, pid, 1, 5, "REPAIRING").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts/repairing-batches", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repairing-batches: {env}");
    assert_eq!(env["code"], 0);
}
