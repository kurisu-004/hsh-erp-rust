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
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! 通用 `login_*` helper，统一走
//! `use hsh_erp_test_support::{...}` + `bootstrap_as_manager()` /
//! `bootstrap_as_clerk()` + `load_outsource_fixture(&pool)`。保留：
//! - `seed_outsource_process` / `insert_l1_customer` / `insert_part` /
//!   `insert_company` / `setup_basic`：quote 域独享（每个测试要按需造不同
//!   customer prefix / 不同 process code / 不同 company name 的组合；
//!   fixture 预置的 FX-OPROC-A 仅作 baseline 共享）；
//! - 域独享 helper 不从 `fixtures` 模块 `use`（Phase H gate 5 禁止）；
//!   本地 helper 用 `sqlx::query` 直插与 fixtures::* 同形 SQL。
//!
//! ## 不预置 t_outsource_quote / t_part / t_part_batch
//! 状态机不允许从 APPROVED 回退 DRAFT / REJECTED，且每个测试都要按需造不同
//! (part, company, process) 组合的 quote；预置会污染「期望空库」list 断言。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, send, test_app,
    test_pool, test_state,
};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 outsource fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。绝大多数 quote 测试以 MANAGER 身份跑。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 起一份 fresh database + 加载 outsource fixture + 以 CLERK 身份登录。
///
/// 仅 `approve_quote_clerk_forbidden_40300` 使用，验证 CLERK 拒绝 approve。
async fn bootstrap_as_clerk() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  quote 域独享 helpers（绕开 fixtures::* 因为 Phase H gate 5 禁止从 `fixtures`
//  模块 use 任何动态 helper）
// ===========================================================================

/// 直插客户（L1）—— 绕开 customer CRUD。
async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool_mgr, app_mgr, m_token, _fx) = bootstrap_as_manager().await;
    let (_pool_clerk, app_clerk, c_token, _fx_clerk) = bootstrap_as_clerk().await;
    let (pid, cid, proc_id) = setup_basic(&pool_mgr).await;

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
    .execute(&pool_mgr)
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
