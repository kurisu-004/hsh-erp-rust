//! part 域 Phase 1（2026-09-13）批次集成测试：1.5 拆分/取消 + 1.6 事件/位置树。
//!
//! 覆盖：
//!   - split_batch: happy path + 数量校验 + 批次守恒不变量
//!   - cancel_batch: happy path + 终态保护
//!   - list_batches: 工单全部活跃批次
//!   - list_events: 工单事件历史
//!   - location_tree: 位置树聚合
//!
//! ## 批次守恒不变量测试
//! `Σ(未删批次.quantity) = t_part.quantity` 必须保持 —— 用 `invariant` 命名空间测试。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::*;

// ===========================================================================
//  动态 part/batch 插入 helper（sub-file 私有，PR-C 末统一迁）
// ===========================================================================

async fn insert_part_with_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    status: &str,
    qty: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let batch_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, NULL, $2, 'D-001', $3, $6, $2, $4, $4, 1, 0, $5, $5)",
    )
    .bind(part_id)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, $4, 0, $5, $5)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(qty)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    (part_id, batch_id)
}

async fn insert_extra_batch(pool: &PgPool, part_id: i64, batch_no: i32, qty: i32, status: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let batch_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(batch_no)
    .bind(qty)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert extra batch");
    batch_id
}

async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
}

// ===========================================================================
//  bootstrap helpers
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  拆分 / 取消 / 列表
// ===========================================================================

#[tokio::test]
async fn split_batch_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 10).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "3",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "split: {env}");
    assert_eq!(env["code"], 0);
    let new_batch_id = env["data"].as_i64().expect("data is i64");
    assert!(new_batch_id > 0);
}

#[tokio::test]
async fn split_batch_invalid_quantity_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 10).await;
    let version = batch_version(&pool, bid).await;
    // quantity == batch.quantity (不允许，等于整批)
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "10",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "quantity=全量应拒绝: {env}");
    assert_eq!(env["code"], 20111);
}

#[tokio::test]
async fn split_batch_quantity_negative_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 10).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "-1",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "quantity<0 应拒绝: {env}");
    assert_eq!(env["code"], 20111);
}

// ===== 批次守恒不变量测试 =====

#[tokio::test]
async fn invariant_split_preserves_total_quantity() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 10).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "3",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "split: {env}");
    // 不变量：Σ quantity == 10
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(quantity), 0)::bigint FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL",
    )
    .bind(pid)
    .fetch_one(&pool)
    .await
    .expect("sum quantity");
    assert_eq!(total, 10, "拆批前后总件数必须守恒 (10=3+7): {env}");
}

#[tokio::test]
async fn cancel_batch_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/{bid}/cancel"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel-batch: {env}");
    assert_eq!(env["code"], 0);
}

#[tokio::test]
async fn cancel_batch_terminal_protection() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "COMPLETED", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/{bid}/cancel"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "COMPLETED 批次禁止取消: {env}");
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn list_batches_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, _bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    insert_extra_batch(&pool, pid, 2, 3, "PENDING").await;
    let (s, env) = send(
        app,
        json_request("GET", &format!("/parts/{pid}/batches"), None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list_batches: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"].as_array().expect("data is array");
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn list_events_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, _bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 1).await;
    let (s, env) = send(
        app,
        json_request("GET", &format!("/parts/{pid}/events"), None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list_events: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"].as_array().expect("data is array");
    assert!(items.is_empty(), "新工单无事件");
}

#[tokio::test]
async fn location_tree_happy_path() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts/location-tree", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "location-tree: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("data.items");
    assert!(
        !items.is_empty(),
        "至少返回 OFFICE / PRODUCTION_SHELF 等父节点"
    );
}