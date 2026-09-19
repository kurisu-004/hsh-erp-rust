//! 2026-09-16 PR-2（part-slim-down）回归测试 —— worker / shelf 停用守卫
//! 改查 t_part_batch 真相源（PR-2 § shelf/repo.rs::count_in_use_parts、
//! worker/repo.rs::count_in_use_parts）。
//!
//! PR-2 之前：「被 X 引用」查 `t_part.current_holder_id`（已删列）。
//! PR-2 之后：改查 `t_part_batch.current_holder_id + location + status`：
//!   - shelf：location IN ('PRODUCTION_SHELF','INSPECTION_SHELF') + status IN
//!     ('IN_PROCESS','INSPECTION','REPAIRING')
//!   - worker：location='WORKER' + status IN
//!     ('IN_PROCESS','INSPECTION','REPAIRING','RETURNED')
//!
//! 任一 >0 ⇒ 20503 BIZ_SHELF_IN_USE / 20203 BIZ_WORKER_IN_USE。
//!
//! 本测试覆盖：
//! 1. shelf 被活跃 IN_PROCESS 批次持有 → deactivate 拒（20503）
//! 2. worker 被活跃 IN_PROCESS 批次持有 → deactivate 拒（20203）

#[path = "common/mod.rs"]
mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, insert_user_with_password, test_app, test_state,
};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use sqlx::PgPool;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    common::ensure_database_exists().await;
    let pool = common::test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

async fn send(
    app: axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (StatusCode, Value) {
    use axum::body::to_bytes;
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
) -> axum::http::Request<axum::body::Body> {
    use axum::body::Body;
    use axum::http::header::AUTHORIZATION;
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
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

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;
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

async fn insert_l2_customer(pool: &PgPool) -> (i64, i64) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let l1 = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(l1)
    .bind("WSDA-L1")
    .bind("F")
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1");
    let l2 = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, $3, NULL, 0, $4, $4)",
    )
    .bind(l2)
    .bind("WSDA-L2")
    .bind(l1)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2");
    (l1, l2)
}

async fn insert_part(pool: &PgPool, customer_id: i64, name: &str, serial_no: Option<&str>) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-WSDA', 'tester', $4, $5, $5, 'IN_PROCESS', 1, 0, $6, $6)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 插 t_part_batch（status / location / holder 由 caller 指定）。
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    status: &str,
    location: &str,
    holder_id: Option<i64>,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 1, $3, $4, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(location)
    .bind(holder_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

async fn insert_worker_min(pool: &PgPool, badge: &str, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, true, NULL, 0, $4, $4)",
    )
    .bind(id)
    .bind(badge)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. shelf 被 IN_PROCESS 批次持有 → deactivate 拒（20503 BIZ_SHELF_IN_USE）。
///
/// PR-2 § shelf/repo.rs::count_in_use_parts：
/// `SELECT COUNT(*) FROM t_part_batch WHERE current_holder_id = $1
///    AND location IN ('PRODUCTION_SHELF','INSPECTION_SHELF')
///    AND status = 'IN_PROCESS' AND deleted_at IS NULL`。
#[tokio::test]
async fn shelf_deactivate_rejects_when_held_by_active_batch() {
    let (_guard, pool) = setup().await;
    let (_l1, l2) = insert_l2_customer(&pool).await;
    let shelf_id = common::insert_shelf(&pool, "WSDA-SHELF", "WSDA", "PRODUCTION").await;

    let pid = insert_part(&pool, l2, "P0", Some("P0-SN")).await;
    // 让该 part 的活跃批次持有该 shelf（IN_PROCESS + PRODUCTION_SHELF）
    insert_batch(&pool, pid, "IN_PROCESS", "PRODUCTION_SHELF", Some(shelf_id)).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/shelves/{shelf_id}/deactivate"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "shelf 被 IN_PROCESS 批次持有 → deactivate 应拒: {env}"
    );
    assert_eq!(
        env["code"], 20503,
        "BIZ_SHELF_IN_USE（PR-2 count_in_use_parts 改 t_part_batch 后应仍命中）: {env}"
    );
}

/// 2. worker 被 IN_PROCESS 批次持有 → deactivate 拒（20203 BIZ_WORKER_IN_USE）。
///
/// PR-2 § worker/repo.rs::count_in_use_parts：
/// `SELECT COUNT(*) FROM t_part_batch WHERE current_holder_id = $1
///    AND location = 'WORKER'
///    AND status = 'IN_PROCESS' AND deleted_at IS NULL`。
#[tokio::test]
async fn worker_deactivate_rejects_when_holding_active_batch() {
    let (_guard, pool) = setup().await;
    let (_l1, l2) = insert_l2_customer(&pool).await;
    let worker_id = insert_worker_min(&pool, "WSDA-BC001", "工-WSDA").await;

    let pid = insert_part(&pool, l2, "P0", Some("P0-SN")).await;
    insert_batch(&pool, pid, "IN_PROCESS", "WORKER", Some(worker_id)).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/workers/{worker_id}/deactivate"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "worker 持有 IN_PROCESS 批次 → deactivate 应拒: {env}"
    );
    assert_eq!(
        env["code"], 20203,
        "BIZ_WORKER_IN_USE（PR-2 count_in_use_parts 改 t_part_batch 后应仍命中）: {env}"
    );
}

/// 3. 反例：worker / shelf 无持有 → deactivate 通过（happy path 回归）。
///
/// 避免 PR-2 改查真相源后误把所有 deactivate 都拒。
#[tokio::test]
async fn shelf_deactivate_succeeds_when_no_active_holders() {
    let (_guard, pool) = setup().await;
    let shelf_id = common::insert_shelf(&pool, "WSDA-EMPTY", "WSDA-empty", "PRODUCTION").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/shelves/{shelf_id}/deactivate"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "无持有 → deactivate 应通过: {env}");
    assert_eq!(env["code"], 0);
}

#[tokio::test]
async fn worker_deactivate_succeeds_when_holding_nothing() {
    let (_guard, pool) = setup().await;
    let worker_id = insert_worker_min(&pool, "WSDA-BC002", "工-WSDA-empty").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/workers/{worker_id}/deactivate"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "无持有 → deactivate 应通过: {env}");
    assert_eq!(env["code"], 0);
}
