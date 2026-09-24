//! part 域 Phase 1（2026-09-13）返修闭环集成测试：1.4 端点。
//!
//! 覆盖：
//!   - complete_repair: REPAIRING → IN_PROCESS（PRODUCTION 区）
//!   - complete_repair: REPAIRING → INSPECTION（INSPECTION 区）
//!   - repair_dispatch: 一步式返修下发
//!   - list_repair_batches / list_repairing_batches

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::{json_request, load_part_fixture, login_token, send, test_app,
    test_pool, test_state};

// ===========================================================================
//  动态 fixture helpers（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::{create_chain_for_part / create_step}`
//  引入 2 helper，因 fixtures.rs 本轮被删，复制到本地（同形 sqlx::query 直插）。
// ===========================================================================

/// 为指定 part 建一个最小工艺链（t_part_process_chain），并把 part.process_chain_id 绑回。
async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let chain_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) \
         VALUES ($1, $2, 0, now(), 0, now(), 0)",
    )
    .bind(chain_id)
    .bind(format!("chain-{part_id}"))
    .execute(pool)
    .await
    .expect("insert chain");
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("bind part to chain");
    chain_id
}

/// 在指定 chain 内创建 step（process_id + sort_order）。
async fn create_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let step_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 30, 0, now(), 0, now(), 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(sort_order)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert chain step");
    step_id
}

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

#[tokio::test]
async fn complete_repair_to_process_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "REPAIRING", 5).await;
    let version = batch_version(&pool, bid).await;

    // 2026-09-16 PR-3 批次 step 化：complete-repair / repair-dispatch
    // PRODUCTION 区要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    // 注：fixture 已预置 fx.production_shelf_id ↔ fx.process_id 的映射
    // （t_shelf_process WORK_TYPE_PROCESS_ID），无需 link_shelf_to_process。
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
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
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "REPAIRING", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.inspection_shelf_id.to_string(),
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
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    // PR-3：repair-dispatch PRODUCTION 区要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
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
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 入口：INSPECTION / READY_TO_SHIP / IN_PROCESS / DELIVERED
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "INSPECTION", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.inspection_shelf_id.to_string(),
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
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "CANCELLED", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.inspection_shelf_id.to_string(),
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
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, _bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "DELIVERED", 5).await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts/repair-batches", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repair-batches: {env}");
    assert_eq!(env["code"], 0);
    let _ = (pid, fx); // suppress unused
}

#[tokio::test]
async fn list_repairing_batches_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, _bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "REPAIRING", 5).await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts/repairing-batches", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repairing-batches: {env}");
    assert_eq!(env["code"], 0);
}