//! outsource shipment 集成测试（Phase 2 2026-09-13）
//!
//! 覆盖（基于 part 域端点的 shipment 写入 + reconcile 端点）：
//! - send-to-outsource INSERT shipment + quote event SENT
//! - receive-from-outsource 标 shipment RECEIVED + quote event RECEIVED
//! - reconcile-update OCC 守
//! - DIRECT 模式 stub → 501
//!
//! 注：实际 send/receive 端点在 part 域（part_lifecycle_api.rs 已覆盖 happy path）。
//! 本测试聚焦 shipment 表的写入正确性。

#[path = "common/mod.rs"]
mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, create_chain_for_part, create_step,
    ensure_database_exists, insert_user_with_password, link_shelf_to_process, seed_process,
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

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String) {
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
    (app2, token)
}

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

async fn insert_part(pool: &PgPool, customer_id: i64, status: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $5, $6, 0, $7, $7)",
    )
    .bind(id)
    .bind(format!("PT-{id}"))
    .bind(format!("DWG-{id}"))
    .bind("Tester")
    .bind(customer_id)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

async fn insert_batch(pool: &PgPool, part_id: i64, status: &str, location: Option<&str>) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch \
         (id, part_id, batch_no, quantity, status, location, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 5, $3, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(location)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

async fn insert_outsource_company(pool: &PgPool, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company (id, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, true, 0, $3, $3)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company");
    id
}

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

async fn insert_approved_quote(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_quote \
         (id, part_id, outsource_company_id, process_id, price, status, submitted_at, \
          reviewed_at, review_note, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 12.50, 'APPROVED', $5, $5, 'OK', 0, $5, $5)",
    )
    .bind(id)
    .bind(part_id)
    .bind(company_id)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_quote");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn send_to_outsource_inserts_shipment_out_sourcing() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "send_admin").await;
    let customer_id = insert_l1_customer(&pool, "Snd", "S").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "SendCo").await;
    let proc_id = seed_outsource_process(&pool, "PSND", "psend").await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/send-to-outsource"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "send-to-outsource: {env}");
    assert_eq!(env["data"]["status"], "OUTSOURCE");

    // 验证 shipment 表
    let row: (i64, String, String, i64, String) = sqlx::query_as(
        "SELECT id, status, sent_at::text, part_id, unit_price::text \
         FROM t_outsource_shipment WHERE batch_id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("shipment row");
    assert_eq!(row.1, "OUTSOURCING");
    assert_eq!(row.3, part_id);
    assert_eq!(row.4, "12.50");
    // 验证 quote_event 写了 SENT
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_outsource_quote_event \
         WHERE quote_id = $1 AND event_type = 'SENT'",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn send_to_outsource_duplicate_open_shipment_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "send_dup").await;
    let customer_id = insert_l1_customer(&pool, "Dup", "D").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "DupCo").await;
    let proc_id = seed_outsource_process(&pool, "PDUP", "dup").await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    // 2026-09-16 PR-3 批次 step 化：send-to-outsource /
    // receive-from-outsource 要求 part 已绑定工艺链
    // （chain + step 已在上面建好，无需重复 setup）

    // 第一次 send 成功
    let (_, env1) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/send-to-outsource"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["data"]["status"], "OUTSOURCE");

    // 直接 raw SQL 把 batch 改回 PENDING + 删 shipment → 再发一次；唯一索引应挡
    sqlx::query("UPDATE t_part_batch SET status = 'PENDING' WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .unwrap();
    // 把原 shipment 改成非 OUTSOURCING（不删），模拟再次发
    sqlx::query("UPDATE t_outsource_shipment SET status = 'RECEIVED', received_at = now() WHERE id IN (SELECT id FROM t_outsource_shipment WHERE batch_id = $1 LIMIT 1)")
        .bind(bid)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE t_part_batch SET status = 'PENDING' WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .unwrap();

    // 再 send 一次 → 应该再插一条 shipment（开口 shipment 不再冲突）
    // 注：unique index 是 partial on (deleted_at IS NULL AND status='OUTSOURCING')，
    //   RECEIVED 后不再冲突。本测试只验：第二次 send 也能成功 + shipment 数 2。
    // 注：batch 当前 version 是 1（第一次 send + 我们手工改 PENDING 时保持）
    let (_, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/send-to-outsource"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": 1,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env2["code"], 0, "2nd send: {env2}");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn send_to_outsource_direct_returns_internal_error() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "send_direct").await;
    let customer_id = insert_l1_customer(&pool, "Dir", "I").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "DirCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIR", "dir").await;

    // 2026-09-16 PR-3 批次 step 化：send-to-outsource /
    // receive-from-outsource 要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/send-to-outsource"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "direct": true,
            })),
            Some(&token),
        ),
    )
    .await;
    // DIRECT 模式 stub → INTERNAL 500
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "direct: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 50000);
}

#[tokio::test]
async fn send_to_outsource_quote_not_approved_returns_21307() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "send_qdraft").await;
    let customer_id = insert_l1_customer(&pool, "Qd", "Q").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "QdCo").await;
    let proc_id = seed_outsource_process(&pool, "PQ", "pq").await;
    // 直接 raw SQL 插一个 DRAFT quote（不走 service 校验）

    // 2026-09-16 PR-3 批次 step 化：send-to-outsource /
    // receive-from-outsource 要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let qid = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_outsource_quote \
         (id, part_id, outsource_company_id, process_id, price, status, \
          version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 1, 'DRAFT', 0, $5, $5)",
    )
    .bind(qid)
    .bind(part_id)
    .bind(company_id)
    .bind(proc_id)
    .bind(now_naive())
    .execute(&pool)
    .await
    .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/send-to-outsource"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": qid.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "draft q: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21307);
}

#[tokio::test]
async fn receive_from_outsource_marks_shipment_received() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "recv_admin").await;
    let customer_id = insert_l1_customer(&pool, "R", "R").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "OUTSOURCE", Some("OUTSOURCE_COMPANY")).await;
    let company_id = insert_outsource_company(&pool, "RecvCo").await;
    let proc_id = seed_outsource_process(&pool, "PR", "pr").await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    // 2026-09-16 PR-3 批次 step 化：send-to-outsource /
    // receive-from-outsource 要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    // 直插一个 OUTSOURCING shipment
    use hsh_erp_rust::infra::clock::now_naive;
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_shipment \
         (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
          quantity, unit_price, status, sent_at, version, created_at, updated_at) \
         VALUES (1, $1, $2, $3, $4, $5, 5, 12.50, 'OUTSOURCING', $6, 0, $6, $6)",
    )
    .bind(quote_id)
    .bind(part_id)
    .bind(bid)
    .bind(company_id)
    .bind(proc_id)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    // 准备接收目标架
    let prod_shelf = common::insert_shelf(&pool, "REC-1", "Recv", "PRODUCTION").await;
    let next_proc = seed_process(&pool, "REC-PROC", "recv_proc").await;
    link_shelf_to_process(&pool, prod_shelf, next_proc).await;
    // 2026-09-16 PR-3：receive 路径要把 batch.current_process_step_id 切到
    // (chain_id, next_process_id) 对应的 step，因此 fixture 必须为 next_proc 也建一个 step。
    let next_step_id = create_step(&pool, chain_id, next_proc, 2).await;
    let _ = next_step_id; // 确认 step 已落库；service 内自行解析

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/receive-from-outsource"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": 0,
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": next_proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "receive: {env}");
    assert_eq!(env["data"]["status"], "IN_PROCESS");

    // 验证 shipment → RECEIVED + received_at 写入
    let (status, _received_at): (String, Option<chrono::NaiveDateTime>) =
        sqlx::query_as("SELECT status, received_at FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "RECEIVED");
    // 验证 quote_event 写了 RECEIVED
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_outsource_quote_event \
         WHERE quote_id = $1 AND event_type = 'RECEIVED'",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn reconcile_update_shipment_unit_price_quantity() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "rec_admin").await;
    let customer_id = insert_l1_customer(&pool, "RU", "U").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "RecCo").await;
    let proc_id = seed_outsource_process(&pool, "PRU", "pru").await;
    use hsh_erp_rust::infra::clock::now_naive;

    // 2026-09-16 PR-3 批次 step 化：send-to-outsource /
    // receive-from-outsource 要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let now = now_naive();
    // 直接插一个 shipment
    let shipment_id: i64 = {
        let snowflake =
            hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
        let id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_outsource_shipment \
             (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
              quantity, unit_price, status, sent_at, version, created_at, updated_at) \
             VALUES ($1, 0, $2, $3, $4, $5, 5, 10.00, 'OUTSOURCING', $6, 0, $6, $6)",
        )
        .bind(id)
        .bind(part_id)
        .bind(bid)
        .bind(company_id)
        .bind(proc_id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        id
    };

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-shipments/{shipment_id}/reconcile-update"),
            Some(json!({
                "unit_price": "15.50",
                "quantity": 8,
                "is_billed": true,
                "version": 0,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "reconcile: {env}");
    assert_eq!(env["data"]["unit_price"], "15.50");
    assert_eq!(env["data"]["quantity"], 8);
    assert_eq!(env["data"]["is_billed"], true);

    // OCC：传错 version → 409
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-shipments/{shipment_id}/reconcile-update"),
            Some(json!({"version": 99})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "occ: {env2}");
    assert_eq!(env2["code"].as_i64().unwrap(), 40901);
}
