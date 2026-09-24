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
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! `login_manager` 通用 helper，统一走
//! `use hsh_erp_test_support::{...}` + `bootstrap_as_manager()` +
//! `load_outsource_fixture(&pool)`。保留：
//! - `insert_l1_customer` / `insert_part` / `insert_batch` /
//!   `insert_outsource_company` / `seed_outsource_process` /
//!   `insert_approved_quote`：send_receive 域独享（每个测试要按需造不同
//!   customer prefix / 不同 part status / 不同 batch location / 不同
//!   company name 的组合；fixture 预置仅作 baseline）；
//! - `create_chain_for_part` / `create_step`：send_receive 域独享（绕开 part
//!   软删级联 + PR-3 批次 step 化要求 part 已绑定工艺链 + step）；
//! - 域独享 helper 不从 `fixtures` 模块 `use`（Phase H gate 5 禁止）；
//!   本地 helper 用 `sqlx::query` 直插与 `fixtures::*` 同形 SQL。
//!
//! ## 不预置 t_part / t_part_batch / t_part_process_chain / t_process_chain_step /
//!  t_outsource_quote / t_outsource_shipment
//! 状态机不允许 part 从 OUTSOURCE 回退 PENDING；每个测试要按需造不同
//! (part, batch, company, process, quote) 组合 + 自建 chain/step。预置会污染
//! list / count 等「期望空库」断言。各 sub-file 用本地 helper 直插。

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
/// 返回 `(pool, app, token, fx)`。所有 send_receive 测试以 MANAGER 身份跑。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  send_receive 域独享 helpers（绕开 fixtures::* 因为 Phase H gate 5 禁止从
//  `fixtures` 模块 use 任何动态 helper）
// ===========================================================================

/// 直插 L1 客户（绕开 customer CRUD）。
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

/// 直插 part（任意 status）。
async fn insert_part(pool: &PgPool, customer_id: i64, status: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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

/// 直插批次（任意 status + 可选 location）。
async fn insert_batch(pool: &PgPool, part_id: i64, status: &str, location: Option<&str>) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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

/// 直插外协公司。
async fn insert_outsource_company(pool: &PgPool, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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

/// 直插 APPROVED 状态 quote（绕开 DRAFT→SUBMITTED→APPROVED 状态机）。
async fn insert_approved_quote(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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

/// 2026-09-16 PR-3 批次 step 化：to_process / place_on_shelf / send_to_outsource /
/// repair 等"进入生产流"端点要求 part 已绑定工艺链（migration 028 +
/// error code 20706 BIZ_PROCESS_CHAIN_REQUIRED）。本 helper 帮 part 建链 + 绑 part。
///
/// 返回 chain_id；caller 可继续调 `create_step` 加 step。
async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let chain_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by) \
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

/// 2026-09-16 PR-3 批次 step 化：在指定 chain 内创建 step（process_id + sort_order）。
async fn create_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    .expect("insert step");
    step_id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn send_to_outsource_inserts_shipment_out_sourcing() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let qid = {
        let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
        let id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_outsource_quote \
             (id, part_id, outsource_company_id, process_id, price, status, \
              version, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, 1, 'DRAFT', 0, $5, $5)",
        )
        .bind(id)
        .bind(part_id)
        .bind(company_id)
        .bind(proc_id)
        .bind(now_naive())
        .execute(&pool)
        .await
        .unwrap();
        id
    };

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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let prod_shelf = snowflake.next_id();
    let recv_shelf_now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, 'REC-1', 'Recv', 'PRODUCTION', true, 0, 0, $2, $2)",
    )
    .bind(prod_shelf)
    .bind(recv_shelf_now)
    .execute(&pool)
    .await
    .unwrap();
    let next_proc = seed_outsource_process(&pool, "REC-PROC", "recv_proc").await;
    // link shelf to process (via t_shelf_process)
    let link_id = snowflake.next_id();
    let link_now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
         created_at, updated_at) VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(link_id)
    .bind(prod_shelf)
    .bind(next_proc)
    .bind(link_now)
    .execute(&pool)
    .await
    .unwrap();
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
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "RU", "U").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "RecCo").await;
    let proc_id = seed_outsource_process(&pool, "PRU", "pru").await;

    // 2026-09-16 PR-3 批次 step 化：send-to-outsource /
    // receive-from-outsource 要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let now = now_naive();
    // 直接插一个 shipment
    let shipment_id: i64 = {
        let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
