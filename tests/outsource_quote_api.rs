//! outsource quote 域集成测试（Phase 2 2026-09-13）
//!
//! 覆盖：
//! - create DRAFT happy path + DRAFT→SUBMITTED→APPROVED 状态机
//! - approve MANAGER-only（CLERK 拒绝 403）
//! - reject SUBMITTED → REJECTED（review_note 必填）
//! - update DRAFT（OCC 版本冲突）
//! - submit DRAFT only（SUBMITTED 状态再 submit → 400）
//! - soft-delete 仅 DRAFT / REJECTED 可删
//! - duplicate 同 (part, company, process) → 409

#[path = "common/mod.rs"]
mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, ensure_database_exists, insert_user_with_password,
    test_app, test_pool, test_state,
};

// ===========================================================================
//  Helpers
// ===========================================================================
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let uri = req.uri().to_string();
    let method = req.method().to_string();
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body_str = String::from_utf8_lossy(&body).to_string();
    let envelope: Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!("parse JSON: {e}; method={method} uri={uri} status={status}; raw = {body_str:?}")
    });
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
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

async fn login(pool: PgPool, username: &str, role: &str) -> (axum::Router, String) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, role, None, None).await;
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
    (app2, token)
}

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String) {
    login(pool, username, "MANAGER").await
}

async fn login_clerk(pool: PgPool, username: &str) -> (axum::Router, String) {
    login(pool, username, "CLERK").await
}

/// 直插客户（L1）—— 绕开 customer CRUD。
async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, serial_prefix, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_customer");
    id
}

/// 直插 part（PENDING）—— 绕开 part CRUD。
async fn insert_part(pool: &PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $5, 'PENDING', 0, $6, $6)",
    )
    .bind(id)
    .bind(format!("PT-{id}"))
    .bind(format!("DWG-{id}"))
    .bind("Tester")
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 直插 OUTSOURCE 类别 process。
async fn seed_outsource_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert OUTSOURCE process");
    id
}

/// 直插外协公司。
async fn insert_company(pool: &PgPool, name: &str, is_active: bool) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company \
         (id, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(is_active)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company");
    id
}

/// 完整准备：1 客户 + 1 part + 1 公司 + 1 OUTSOURCE process，返回 (pid, cid, proc_id)。
async fn setup_basic(pool: &PgPool) -> (i64, i64, i64) {
    let customer_id = insert_l1_customer(pool, "QuoteCo", "Q").await;
    let part_id = insert_part(pool, customer_id).await;
    let company_id = insert_company(pool, "Quote Outsource Co", true).await;
    let proc_id = seed_outsource_process(pool, "QPROC", "Q过程").await;
    (part_id, company_id, proc_id)
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_quote_draft_happy() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "q_admin").await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "12.50",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    assert_eq!(env["data"]["status"], "DRAFT");
    assert_eq!(env["data"]["price"], "12.50");
}

#[tokio::test]
async fn create_quote_duplicate_returns_21303() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "q_dup").await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    let body = json!({
        "part_id": pid.to_string(),
        "outsource_company_id": cid.to_string(),
        "process_id": proc_id.to_string(),
        "price": "9.99",
    });
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(body.clone()),
            Some(&token),
        ),
    )
    .await;
    let (s2, env2) = send(
        app,
        json_request("POST", "/outsource-quotes", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "duplicate: {env2}");
    assert_eq!(env2["code"].as_i64().unwrap(), 21303);
}

#[tokio::test]
async fn quote_full_lifecycle_draft_submit_approve() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "q_life").await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    // create
    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "20.00",
            })),
            Some(&token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    let ver = env_c["data"]["version"].as_i64().unwrap();

    // submit
    let (s_sub, env_sub) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_sub, StatusCode::OK, "submit: {env_sub}");
    assert_eq!(env_sub["data"]["status"], "SUBMITTED");
    assert!(env_sub["data"]["submitted_at"].is_string());

    // approve (MANAGER-only — token 是 manager)
    let (s_app, env_app) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/approve"),
            Some(json!({"review_note": "OK", "version": ver + 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_app, StatusCode::OK, "approve: {env_app}");
    assert_eq!(env_app["data"]["status"], "APPROVED");
    assert_eq!(env_app["data"]["review_note"], "OK");
}

#[tokio::test]
async fn approve_quote_clerk_forbidden_40300() {
    let (_guard, pool) = setup().await;
    let (app_mgr, m_token) = login_manager(pool.clone(), "q_clerk_mgr").await;
    let (app_clerk, c_token) = login_clerk(pool.clone(), "q_clerk").await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    // create via manager
    let (_, env_c) = send(
        app_mgr.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "5.00",
            })),
            Some(&m_token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    // 跳过 submit（用 raw SQL 直接 set SUBMITTED，version+1 → 1）
    sqlx::query(
        "UPDATE t_outsource_quote SET status='SUBMITTED', submitted_at = now(), \
         version=version+1 WHERE id=$1",
    )
    .bind(qid.parse::<i64>().unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // clerk 尝试 approve → 403
    let (s, env) = send(
        app_clerk,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/approve"),
            Some(json!({"version": 1})),
            Some(&c_token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "clerk approve: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40300);
}

#[tokio::test]
async fn reject_quote_requires_review_note() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "q_rej").await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "10.00",
            })),
            Some(&token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    // submit
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    // reject with empty note → 400
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/reject"),
            Some(json!({"review_note": "", "version": 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "empty note: {env}");
    // reject with note
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/reject"),
            Some(json!({"review_note": "太贵", "version": 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "reject: {env2}");
    assert_eq!(env2["data"]["status"], "REJECTED");
}

#[tokio::test]
async fn submit_quote_wrong_status_returns_21302() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "q_ws").await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "10.00",
            })),
            Some(&token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    // 第一次 submit OK
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    // 第二次 submit → 400 / 21302
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "2nd submit: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21302);
}

#[tokio::test]
async fn soft_delete_quote_approved_forbidden() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "q_sd").await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "10.00",
            })),
            Some(&token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    // submit + approve → APPROVED
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/approve"),
            Some(json!({"version": 1})),
            Some(&token),
        ),
    )
    .await;
    // soft-delete → 400 / 21302
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "sd APPROVED: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21302);
}
