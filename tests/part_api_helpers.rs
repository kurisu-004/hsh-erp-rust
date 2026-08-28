//! part 域 to-XXX 端点集成测试的共享 fixtures + HTTP helpers
//!
//! 由 `tests/part_api_to_ship.rs` / `part_api_to_inspection.rs` /
//! `part_api_to_process.rs` 通过 `#[path = "part_api_helpers.rs"] mod helpers;`
//! 共用。所有 helper 都 `pub`，便于跨文件 `use helpers::*`。
//!
//! 设计意图：把 `tests/part_api.rs`（原 1337 行）拆成按 to-XXX 端点切分的
//! 顶层测试文件，每个文件 ≤ 1000 行（CLAUDE.md 单文件行数上限）。
//!
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;

use common::{add_role, insert_user_with_password, test_app, test_state};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use tower::ServiceExt;

// ===========================================================================
//  全局串行化 + HTTP helpers
// ===========================================================================

/// 进程级测试串行化（共享 `postgres_rust_test`）。
pub static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 发请求 → 拆响应为 (status, envelope JSON)。测试用 envelope 解析失败立即 panic。
pub async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let envelope: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("parse JSON: {e}; raw = {}", String::from_utf8_lossy(&body)));
    (status, envelope)
}

/// 构造 axum `Request<Body>`：method/uri/bearer/body。
pub fn json_request(method: &str, uri: &str, body: Option<Value>, bearer: Option<&str>) -> Request<Body> {
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

/// 串行化 + 拿 TEST_LOCK + 拿干净 PgPool。
pub async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

// ----- 角色登录 helper（建用户 → 加角色 → 登录拿 token） -----

/// 插 MANAGER 角色用户 + 登录拿 token。
pub async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
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

/// 插 INSPECTOR 角色用户 + 登录拿 token。
pub async fn login_inspector(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "INSPECTOR", None, None).await;
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

/// 插 CLERK 角色用户 + 登录拿 token。
pub async fn login_clerk(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "CLERK", None, None).await;
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

// ===========================================================================
//  Domain fixtures：客户 / 工单 / 批次 / 货架
// ===========================================================================

/// 插 L1 客户（一级；带 serial_prefix）。
pub async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
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

/// 插 L2 客户（二级；挂在 L1 下，无 prefix）。
pub async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
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

/// 带 status 参数的 part 插入（适配 INSPECTION / IN_PROCESS / PENDING / PROGRAMMING / READY_TO_SHIP）。
///
/// 与 `tests/delivery_scan_api.rs::insert_part` 不同：原 fixture 硬编码 `status="INSPECTION"`，
/// 本测试需要构造 partial-failure（混合 INSPECTION / IN_PROCESS），故参数化 status。
pub async fn insert_part_with_status(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    assembly_id: Option<i64>,
    status: &str,
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
         VALUES ($1, $2, $3, 'D-001', $4, $8, $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL, $7)",
        id, serial_no, name, customer_id, now, today, assembly_id, status,
    )
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 插一个 part_batch（带 status 参数，与 part.status 通常对齐）。
pub async fn insert_batch(pool: &PgPool, part_id: i64, batch_no: i32, qty: i32, status: &str) -> i64 {
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

/// to_inspection / to_process 测试用：插一个 INSPECTION 货架 + 一个 PRODUCTION 货架。
///
/// `next_process_id` 是 to_process 需要的占位（service 仅透传，不校验存在性——
/// shelf 域 `t_shelf_process` 映射校验留待后续 shelf 域 PR）。
pub async fn setup_inspection_and_production_shelves(pool: &PgPool) -> (i64, i64, i64) {
    use common::insert_shelf;
    let inspection_shelf = insert_shelf(pool, "INSP-001", "品检架A", "INSPECTION").await;
    let production_shelf = insert_shelf(pool, "PROD-001", "生产架A", "PRODUCTION").await;
    let next_process_id = 999_999_i64;
    (inspection_shelf, production_shelf, next_process_id)
}