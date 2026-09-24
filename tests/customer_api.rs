//! customer 域端到端集成测试
//!
//! ## 覆盖（Phase P1 customer CRUD 段）
//! 1. create L1（带 serial_prefix）+ create L2（带 parent_id）+ soft-delete L1 → 20113
//!    BIZ_CUSTOMER_IN_USE（因为 L1 仍被 t_part 引用）。
//! 2. update L1 的 serial_prefix 与另一 L1 撞 unique → 20104
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（POST /com/customers 写路径要求 M/C，按设计 §6.1 用 M 即可；2026-09-19 聚合到 com nest）。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "common/mod.rs"] mod common;` + `use common::{...};` 改走
//! `use hsh_erp_test_support::*` + `load_customer_fixture(&pool)` +
//! `CustomerFixture` + `bootstrap_as_manager` 样板。fixture 提供 1 MANAGER user
//! baseline（测试内现场创建 L1/L2 customer，避免 fixture 占用 serial_prefix 字面）。
//! 字面请求 / 断言逐字保留。

use sqlx::PgPool;

use hsh_erp_test_support::{
    CustomerFixture, json_request, load_customer_fixture, send as ts_send, test_app, test_pool,
    test_state,
};

// ===========================================================================
//  Helpers
// ===========================================================================

async fn send(
    app: axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, serde_json::Value) {
    ts_send(app, req).await
}

/// 重新构造 Router（oneshot 消耗 Router 之后）。
async fn fresh_app(pool: &PgPool) -> axum::Router {
    test_app(test_state(pool.clone()).await)
}

/// 基础 bootstrap：fresh DB + customer fixture 2 行（baseline MANAGER user）+ 登录拿 token。
async fn bootstrap_as_manager() -> (PgPool, String) {
    let pool = test_pool().await;
    let _fx = load_customer_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(serde_json::json!({"username": CustomerFixture::MANAGER_USERNAME, "password": CustomerFixture::PASSWORD})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    (pool, token)
}

/// 直插一个 `t_part` 行（customer_id = given），让 soft-delete 检查「被 part 引用」分支
/// 触发 `20113 BIZ_CUSTOMER_IN_USE`。绕开 part 域 CRUD（part CRUD 不是本任务范畴）。
async fn insert_part_with_customer(pool: &PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, version, created_at, updated_at) \
         VALUES ($1, 'TEST-NAME', 'TEST-DWG', 'TEST-APPLICANT', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $2, 0, $3, $3)",
        id,
        customer_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part referencing customer");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_customer_root_then_l2_then_soft_delete_in_use() {
    let (pool, token) = bootstrap_as_manager().await;

    // Create L1
    let (s1, env1) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({"name": "ACME", "serial_prefix": "A"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, axum::http::StatusCode::CREATED, "create L1: {env1}");
    assert_eq!(env1["code"], 0);
    let l1_id = env1["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(env1["data"]["serial_prefix"], "A");

    // Create L2 (parent_id = l1_id)
    let (s2, env2) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({
                "name": "ACME-Workshop1",
                "parent_id": l1_id.clone(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, axum::http::StatusCode::CREATED, "create L2: {env2}");
    assert_eq!(env2["code"], 0);
    assert_eq!(env2["data"]["parent_id"], l1_id);

    // Insert a t_part referencing L1 directly so soft-delete check fires
    // (brief's service logic only inspects t_part / t_assembly).
    insert_part_with_customer(&pool, l1_id.parse::<i64>().unwrap()).await;

    // Soft-delete L1 → should fail with 20113 BIZ_CUSTOMER_IN_USE
    let (s3, env3) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/com/customers/{l1_id}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s3,
        axum::http::StatusCode::CONFLICT,
        "soft-delete should return 409 CONFLICT for BIZ_CUSTOMER_IN_USE; got {env3}"
    );
    assert_eq!(
        env3["code"].as_i64().unwrap(),
        20113,
        "expected BIZ_CUSTOMER_IN_USE; got envelope: {env3}"
    );
}

#[tokio::test]
async fn update_customer_serial_prefix_collision_returns_20104() {
    let (pool, token) = bootstrap_as_manager().await;

    // Create two L1 customers with distinct serial_prefix values.
    let (_s1, env1) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({"name": "Alpha", "serial_prefix": "A"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    let l1_a_id = env1["data"]["id"].as_str().unwrap().to_string();

    let (_s2, env2) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({"name": "Bravo", "serial_prefix": "B"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env2["code"], 0);

    // Try to rename A → "B". uq_t_customer_root_prefix fires → 23505 → 20104.
    let (s3, env3) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/com/customers/{l1_a_id}/update"),
            Some(serde_json::json!({"serial_prefix": "B"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s3,
        axum::http::StatusCode::BAD_REQUEST,
        "duplicate-prefix update should return 400 BIZ_INVALID_VALUE; got {env3}"
    );
    assert_eq!(
        env3["code"].as_i64().unwrap(),
        20104,
        "expected BIZ_INVALID_VALUE (20104); got envelope: {env3}"
    );
    assert!(
        env3["message"]
            .as_str()
            .unwrap_or_default()
            .contains("serial_prefix 已存在"),
        "expected message to contain 'serial_prefix 已存在'; got: {env3}"
    );
}