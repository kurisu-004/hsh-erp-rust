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
use hsh_erp_test_support::{
    json_request, load_part_fixture, login_token, send, test_app, test_pool, test_state,
};

// ===========================================================================
//  动态 fixture helpers（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::{create_chain_for_part / create_step}`
//  引入 2 helper，因 fixtures.rs 本轮被删，复制到本地（同形 sqlx::query 直插）。
// ===========================================================================

/// 为指定 part 建一个最小工艺链（t_part_process_chain），并把 part.process_chain_id 绑回。
async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
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
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
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

// ===========================================================================
//  回归测试（2026-09-30，review 第 3 轮 M3 补漏项）
// ===========================================================================

/// 造出**真实送检后形态**的 DELIVERED 批次：`current_process_step_id` 非空
/// （送检期间被刻意保留）+ `current_process_id` 显式 NULL。
///
/// 为什么必须是这个形态：进 `READY_TO_SHIP` 的迁移边**只有**
/// `INSPECTION → READY_TO_SHIP`，进 `DELIVERED` 的边**只有**
/// `READY_TO_SHIP → DELIVERED`；而所有进 INSPECTION 的写点都把
/// `current_process_id` 置 NULL，其后 `mark_batch_passed_inspection` 与
/// `mark_batch_delivered` 都不写该列 ⇒ DELIVERED 批次该列**结构性恒 NULL**。
/// 换言之，真实生产库里没有「DELIVERED 批次带 cpid」这种数据形态。
///
/// 返回 `(part_id, batch_id, process_id, process_name)`。
#[allow(clippy::too_many_arguments)]
async fn insert_step_located_delivered_part_batch(
    pool: &PgPool,
    part_name: &str,
    customer_id: i64,
    serial_no: &str,
    process_code: &str,
    process_name: &str,
) -> (i64, i64, i64, String) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let today = now.date();

    // 1. 工序（列表查询只 JOIN t_process 取名，不需 t_shelf_process 映射）
    let process_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
    )
    .bind(process_id)
    .bind(process_code)
    .bind(process_name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");

    // 2. 工艺链 + step（step 指向该工序）
    let chain_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, $2, 0, $3, 0, $3, 0)",
    )
    .bind(chain_id)
    .bind(format!("chain-{process_code}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_process_chain");
    let step_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 30, 0, $4, 0, $4, 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process_chain_step");

    // 3. DELIVERED part（绑上 chain，与真实数据一致）
    let part_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at, process_chain_id) \
         VALUES ($1, $2, $3, 'D-REPAIR-001', $4, 'DELIVERED', $3, $5, $5, 1, 0, $6, $6, $7)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(part_name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(chain_id)
    .execute(pool)
    .await
    .expect("insert DELIVERED part with chain");

    // 4. DELIVERED 批次：step 有值、cpid **显式 NULL**（真实送检后形态）
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, \
         current_process_id, current_process_step_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 1, 'DELIVERED', NULL, $3, 0, $4, $4)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(step_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert step-located DELIVERED batch");

    (part_id, batch_id, process_id, process_name.to_string())
}

/// `GET /parts/repair-batches`：`next_process_id` / `next_process_name` 必须由
/// `current_process_step_id` → step JOIN 派生，**不直读** `current_process_id`。
///
/// 若有人把 `list_batches_with_status` 改回直读 cpid，本测试必红
/// （DELIVERED 必经 INSPECTION → cpid 恒 NULL → 两字段变 null）。
#[tokio::test]
async fn repair_batches_derives_next_process_from_step_not_cpid() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_part_id, batch_id, process_id, process_name) = insert_step_located_delivered_part_batch(
        &pool,
        "PART_REPAIR_STEP",
        fx.customer_l2_id,
        "P-RPR-001",
        "PROC-RPR-STEP",
        "返修显示工序",
    )
    .await;

    // 前置断言：DB 层确实是「step 有值 + cpid 为 NULL」的真实 DELIVERED 形态
    let (step_opt, cpid): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_step_id, current_process_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("read batch process columns");
    assert!(
        step_opt.is_some(),
        "前置条件：current_process_step_id 应非空（送检期间保留），实际 {step_opt:?}"
    );
    assert_eq!(
        cpid, None,
        "前置条件：current_process_id 应为 NULL（DELIVERED 必经 INSPECTION = 出池），\
         实际 {cpid:?}；若本断言失败说明出池不变式被回退，测试前提已变"
    );

    let (s, env) = send(
        app,
        json_request("GET", "/parts/repair-batches?limit=50", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repair-batches: {env}");
    assert_eq!(env["code"], 0);

    let items = env["data"]["items"].as_array().expect("data.items");
    let batch_id_str = batch_id.to_string();
    let process_id_str = process_id.to_string();
    let hit = items
        .iter()
        .find(|i| i["batch_id"] == batch_id_str)
        .unwrap_or_else(|| panic!("items 应含 batch_id={batch_id}: {env}"));

    assert_eq!(
        hit["next_process_id"], process_id_str,
        "next_process_id 必须由 current_process_step_id → step JOIN 派生为该 step 的 \
         process_id（{process_id}）。实际 {:?} —— 若为 null 说明查询被改成直读 \
         current_process_id，而 DELIVERED 必经 INSPECTION（=出池）已把该列置 NULL \
         （review M3 同形回归）",
        hit["next_process_id"]
    );
    assert_eq!(
        hit["next_process_name"].as_str(),
        Some(process_name.as_str()),
        "next_process_name 应由同一条 step JOIN 链解析出 {process_name}: {env}"
    );
    assert_eq!(
        hit["current_process_step_id"],
        step_opt.unwrap().to_string(),
        "current_process_step_id 原样透出（它是 next_process_id 的派生源）: {env}"
    );
}

/// `GET /parts/repairing-batches` 走的是**同一条** SQL（`statuses = ['REPAIRING']`），
/// 故也需 guard。与 DELIVERED 的差别在 cpid 语义：`mark_batch_repairing` 既不写
/// 也不清该列（迁移 004「已知局限 4d」），真实 REPAIRING 批次带的是**进返修前
/// 那道工序的陈旧值**。本测试显式造出「step 有值 + cpid 为陈旧非空值」的形态，
/// 断言端点仍返回 step 派生的工序（而非那个陈旧值）—— 若改回直读必红。
#[tokio::test]
async fn repairing_batches_derives_next_process_from_step_not_stale_cpid() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (part_id, batch_id, process_id, process_name) = insert_step_located_delivered_part_batch(
        &pool,
        "PART_REPAIRING_STEP",
        fx.customer_l2_id,
        "P-RPR-002",
        "PROC-RPRING-STEP",
        "返修中显示工序",
    )
    .await;

    // 改造成 REPAIRING，并写入一个**陈旧的 cpid**（模拟 4d：进返修不清该列）
    let stale_cpid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES (9000000000000099901, 'PROC-STALE', '陈旧工序', 'INHOUSE', 0, false, 0, \
                 now(), now()) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert stale process");
    sqlx::query(
        "UPDATE t_part_batch SET status = 'REPAIRING', current_process_id = $2 WHERE id = $1",
    )
    .bind(batch_id)
    .bind(stale_cpid)
    .execute(&pool)
    .await
    .expect("flip batch to REPAIRING with stale cpid");

    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/parts/repairing-batches?limit=50",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repairing-batches: {env}");
    assert_eq!(env["code"], 0);

    let items = env["data"]["items"].as_array().expect("data.items");
    let batch_id_str = batch_id.to_string();
    let hit = items
        .iter()
        .find(|i| i["batch_id"] == batch_id_str)
        .unwrap_or_else(|| panic!("items 应含 batch_id={batch_id}: {env}"));

    assert_eq!(
        hit["next_process_id"],
        process_id.to_string(),
        "next_process_id 必须由 step JOIN 派生（{process_id}），而非 REPAIRING \
         批次残留的陈旧 cpid（{stale_cpid}）。实际 {:?}",
        hit["next_process_id"]
    );
    assert_eq!(
        hit["next_process_name"].as_str(),
        Some(process_name.as_str()),
        "next_process_name 应为 step 派生的 {process_name}，而非陈旧的「陈旧工序」: {env}"
    );
    let _ = part_id;
}
