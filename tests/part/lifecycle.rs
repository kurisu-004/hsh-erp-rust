//! part 域 Phase 1（2026-09-13）生命周期集成测试：1.1/1.2/1.3/1.4/1.7 端点。
//!
//! 覆盖：
//!   - place-on-shelf: PENDING → IN_PROCESS（happy + RBAC + 状态机拒绝 + shelf↔process 校验）
//!   - recall: IN_PROCESS（在生产架 / 工人持有）/ PROGRAMMING → PENDING
//!     （happy + 出池四列清空 + 非生产位置拒绝）。2026-10-08 端点自
//!     `POST /prod/batches/{batch_id}/recall-to-pending` 迁到
//!     `POST /prod/queue/recall`（`batch_id` 改入 body、出参改 `RecallOut`），
//!     用例随之改打新路径。
//!   - release-from-programming: PROGRAMMING → IN_PROCESS（happy + RBAC）
//!   - send-to-outsource: PENDING → OUTSOURCE
//!   - receive-from-outsource: OUTSOURCE → IN_PROCESS
//!   - receive-from-outsource-to-inspection: OUTSOURCE → INSPECTION
//!   - complete-repair: 返修中 → IN_PROCESS / INSPECTION
//!     （2026-10-01：REPAIRING 降级为 `t_part_batch.is_repairing` 标记列，
//!     返修中批次 = `status='IN_PROCESS' + is_repairing=true`）
//!   - repair-dispatch: 一步式返修下发
//!   - scan-inspect: 一步式扫码品检（PASS/FAIL）
//!   - scan-deliver-part: 司机扫码发货
//!
//! 2026-09-29 端点下线：`send-to-programming` / `recall-to-programming` 整体
//! 移除测试（PROGRAMMING 状态废弃进入路径；详见
//! `src/modules/part/statemachine.rs::can_transition_to`）。
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。

use axum::http::StatusCode;
use serde_json::{Value, json};
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

async fn bootstrap_as_clerk() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  1.1 place-on-shelf 测试
// ===========================================================================

#[tokio::test]
async fn place_on_shelf_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    // PR-3: 建链并绑 part
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    // fixture 已预置映射
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/place-on-shelf"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "place-on-shelf: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "IN_PROCESS");
}

#[tokio::test]
async fn place_on_shelf_rbac_clerk_ok() {
    let (pool, app, token, fx) = bootstrap_as_clerk().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    // fixture 已预置映射
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/place-on-shelf"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "clerk should be allowed: {env}");
    assert_eq!(env["code"], 0);
}

#[tokio::test]
async fn place_on_shelf_invalid_transition_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 工单 COMPLETED 状态（place-on-shelf 要求 PENDING）
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "COMPLETED", 5).await;
    let version = batch_version(&pool, bid).await;
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    // fixture 已预置映射
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/place-on-shelf"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "COMPLETED → IN_PROCESS 应被状态机拒绝: {env}"
    );
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn place_on_shelf_shelf_process_not_mapped_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    // 删除 fixture 预置的 shelf↔process 映射 → 20507 BIZ_SHELF_PROCESS_NOT_MAPPED
    sqlx::query("DELETE FROM t_shelf_process WHERE shelf_id = $1 AND process_id = $2")
        .bind(fx.production_shelf_id)
        .bind(fx.process_id)
        .execute(&pool)
        .await
        .expect("delete shelf_process mapping");
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/place-on-shelf"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    // 20507 BIZ_SHELF_PROCESS_NOT_MAPPED → Phase 2 (2026-09-13) 显式映射 422
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "shelf↔process 缺失应拒绝: {env}"
    );
    assert_eq!(env["code"], 20507);
}

#[tokio::test]
async fn recall_to_pending_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 5).await;
    // 写入 location=PRODUCTION_SHELF（recall 要求批次停在生产中位置）
    sqlx::query("UPDATE t_part_batch SET location = 'PRODUCTION_SHELF' WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .expect("set location");
    let version = batch_version(&pool, bid).await;
    // 2026-10-08：`batch_id` 由 path 参数改入 body（传字符串形态，与前端一致）
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request("POST", "/prod/queue/recall", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "recall: {env}");
    assert_eq!(env["code"], 0);
    // 2026-10-08 出参改本域 VO（原返 part 全量投影 PartOut）
    assert_eq!(env["data"]["batch_id"], bid.to_string(), "{env}");
    assert_eq!(
        env["data"]["version"],
        version + 1,
        "version 应为写入后的值: {env}"
    );
    let (status,): (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .expect("read status after recall");
    assert_eq!(status, "PENDING", "批次应已翻到 PENDING");
}

// ===========================================================================
//  1.2 CNC 编程流转测试
// ===========================================================================

#[tokio::test]
async fn release_from_programming_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PROGRAMMING", 5).await;
    let version = batch_version(&pool, bid).await;
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    // fixture 已预置映射
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/release-from-programming"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "release: {env}");
    assert_eq!(env["data"]["status"], "IN_PROCESS");
}

#[tokio::test]
async fn release_from_programming_rbac_inspector_rejects() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PROGRAMMING", 5).await;
    let version = batch_version(&pool, bid).await;
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    // fixture 已预置映射
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/release-from-programming"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "INSPECTOR 不应能 release-from-programming: {env}"
    );
    assert_eq!(env["code"], 40300);
}

// ===========================================================================
//  1.7 扫码检 / 司机扫码
// ===========================================================================

#[tokio::test]
async fn scan_inspect_pass_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "pass": true,
        "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/scan-inspect"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan-inspect PASS: {env}");
    assert_eq!(env["data"]["status"], "READY_TO_SHIP");
}

#[tokio::test]
async fn scan_inspect_fail_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 5).await;
    let version = batch_version(&pool, bid).await;
    // 设 location=PRODUCTION_SHELF + current_holder_id=production_shelf
    sqlx::query(
        "UPDATE t_part_batch SET location = 'PRODUCTION_SHELF', current_holder_id = $1 \
         WHERE id = $2",
    )
    .bind(fx.production_shelf_id)
    .bind(bid)
    .execute(&pool)
    .await
    .unwrap();
    let body = json!({
        "pass": false,
        "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/scan-inspect"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan-inspect FAIL: {env}");
    // 2026-10-01：REPAIRING 降级为 t_part_batch.is_repairing 标记，
    // t_part.status 保持 IN_PROCESS（返修仍在生产中，progress 同档）。
    assert_eq!(env["data"]["status"], "IN_PROCESS");
    let is_repairing: bool =
        sqlx::query_scalar("SELECT is_repairing FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("read is_repairing");
    assert!(
        is_repairing,
        "scan-inspect FAIL 应置 t_part_batch.is_repairing = true"
    );
}

#[tokio::test]
async fn scan_inspect_invalid_transition_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "DELIVERED", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "pass": true,
        "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/scan-inspect"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "DELIVERED 起点不允许: {env}");
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn scan_deliver_part_requires_driver() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 创建 part 带 serial_no
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let pid = snowflake.next_id();
    let bid = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, 'B001-001', 'P0', 'D-001', $2, 'READY_TO_SHIP', 'P0', $3, $3, 1, 0, $4, $4)",
    )
    .bind(pid)
    .bind(fx.customer_l2_id)
    .bind(today)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert part with serial_no");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, 5, 'READY_TO_SHIP', 0, $3, $3)",
    )
    .bind(bid)
    .bind(pid)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert batch");
    let _version = batch_version(&pool, bid).await;
    // 不创建任何 worker → 找不到工牌 → 401/404 错
    let body = json!({
        "part_serial_no": "B001-001",
        "worker_badge_code": "BADGE-001",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/scan/deliver",
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "无效工牌应拒绝: {env}");
    assert_eq!(env["code"], 20201);
}

// ===========================================================================
//  2026-10-01：status_gate 单一写入口 + 终态序列号释放
// ===========================================================================

/// 造一个「两条 DELIVERED 批次 + 带 serial_no」的工单。
///
/// 返回 (part_id, batch1_id, batch2_id, serial_no)。
async fn insert_part_with_two_delivered_batches(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
) -> (i64, i64, i64, String) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 11);
    let part_id = snowflake.next_id();
    let b1 = snowflake.next_id();
    let b2 = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 'P-MULTI', 'D-001', $3, 'DELIVERED', 'P-MULTI', $4, $4, 2, 0, $5, $5)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");
    for (idx, bid) in [b1, b2].iter().enumerate() {
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $3, 1, 'DELIVERED', 0, $4, $4)",
        )
        .bind(bid)
        .bind(part_id)
        .bind(idx as i32 + 1)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert batch");
    }
    (part_id, b1, b2, serial_no.to_string())
}

async fn part_serial_no(pool: &PgPool, part_id: i64) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT serial_no FROM t_part WHERE id = $1")
        .bind(part_id)
        .fetch_one(pool)
        .await
        .expect("read serial_no")
}

async fn part_status(pool: &PgPool, part_id: i64) -> String {
    sqlx::query_scalar::<_, String>("SELECT status FROM t_part WHERE id = $1")
        .bind(part_id)
        .fetch_one(pool)
        .await
        .expect("read part status")
}

/// 2026-10-01 回归测试：多批次工单的序列号释放时机。
///
/// 覆盖本次修的核心缺陷（`clear_part_serial_no_when_completed` 的 WHERE 是
/// **part 级** `status='COMPLETED'`，而 `complete` 一次只翻**一条**批次，
/// 于是该 UPDATE 命中 0 行并被 `let _ =` 静默吞掉 → 序列号被
/// `uk_t_part_serial_no` 永久占住）：
///
/// 1. 完成第 1 条批次 → part 仍 DELIVERED（min-progress 还是有别的批次）→
///    **序列号必须仍在**（货还在厂里，唯一索引继续占用是**正确**行为）；
/// 2. 完成第 2 条批次 → part 被 rollup 进 COMPLETED → 序列号**必须被释放**，
///    且 `t_part_event` 留下一条 `SERIAL_RELEASED` 归档（note 含原序列号）；
/// 3. 释放后同一序列号可被新工单复用（`uk_t_part_serial_no` 不再拦）。
#[tokio::test]
async fn multi_batch_complete_releases_serial_only_when_part_terminates() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, b1, b2, serial) =
        insert_part_with_two_delivered_batches(&pool, fx.customer_l2_id, "SN-MULTI-0001").await;

    // ---- 第 1 步：完成批次 1（part 还没到终态）----
    let v1 = batch_version(&pool, b1).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{b1}/complete"),
            Some(json!({  "version": v1 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete batch1: {env}");
    assert_eq!(
        part_status(&pool, pid).await,
        "DELIVERED",
        "还有一条 DELIVERED 批次，part 不应到 COMPLETED"
    );
    assert_eq!(
        part_serial_no(&pool, pid).await,
        Some(serial.clone()),
        "part 未到终态，序列号必须仍被占用（唯一索引 uk_t_part_serial_no 继续占坑）"
    );

    // ---- 第 2 步：完成批次 2（part 派生到 COMPLETED → 释放）----
    let v2 = batch_version(&pool, b2).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{b2}/complete"),
            Some(json!({  "version": v2 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete batch2: {env}");
    assert_eq!(part_status(&pool, pid).await, "COMPLETED");
    assert_eq!(
        part_serial_no(&pool, pid).await,
        None,
        "part 到 COMPLETED，序列号必须被释放（原实现在这里永久泄漏）"
    );

    // 归档事件：note 必须含原序列号
    let note: Option<String> = sqlx::query_scalar(
        "SELECT note FROM t_part_event \
         WHERE part_id = $1 AND event_type = 'SERIAL_RELEASED'",
    )
    .bind(pid)
    .fetch_optional(&pool)
    .await
    .expect("query SERIAL_RELEASED")
    .flatten();
    let note = note.expect("应有 1 条 SERIAL_RELEASED 归档事件");
    assert!(
        note.contains(&serial),
        "归档 note 应含原序列号 {serial}，实际 {note}"
    );

    // 释放后可被新工单复用（唯一索引不再拦）
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let sf = SnowflakeIdGenerator::new(1_577_836_800_000, 12);
    let new_part = sf.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 'P-REUSE', 'D-001', $3, 'PENDING', 'P-REUSE', $4, $4, 1, 0, $5, $5)",
    )
    .bind(new_part)
    .bind(&serial)
    .bind(fx.customer_l2_id)
    .bind(now.date())
    .bind(now)
    .execute(&pool)
    .await
    .expect("释放后同序列号应可被新工单复用（uk_t_part_serial_no 不再拦）");
}

/// 2026-10-01 回归测试：最后一条活跃批次被**取消**（不是完成）导致 part
/// 派生到 CANCELLED 时，序列号也必须被释放。
///
/// 覆盖改造前的**永久泄漏**路径：`PartService::cancel_batch`
/// （`POST /prod/batches/{batch_id}/cancel`）只翻批次、调 rollup，
/// 从不调 `clear_part_serial_no_when_completed`（那个函数的 WHERE 只认
/// `status='COMPLETED'`）。于是 part 已 CANCELLED、`serial_no` 却还挂着，
/// 被 `uk_t_part_serial_no` 永久占住，同序列号再也无法被新工单复用，
/// 且**没有任何报错或告警**。
#[tokio::test]
async fn cancel_last_batch_releases_serial_when_part_becomes_cancelled() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, b1, b2, serial) =
        insert_part_with_two_delivered_batches(&pool, fx.customer_l2_id, "SN-CANCEL-0001").await;

    // 取消第 1 条：part 还有一条 DELIVERED 批次 → 不是终态 → 序列号继续占用
    let v1 = batch_version(&pool, b1).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{b1}/cancel"),
            Some(json!({ "version": v1 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel batch1: {env}");
    assert_eq!(part_status(&pool, pid).await, "DELIVERED");
    assert_eq!(
        part_serial_no(&pool, pid).await,
        Some(serial.clone()),
        "part 未到终态，序列号应仍被占用"
    );

    // 取消第 2 条：全部批次 CANCELLED → part 派生到 CANCELLED → 必须释放
    let v2 = batch_version(&pool, b2).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{b2}/cancel"),
            Some(json!({ "version": v2 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel batch2: {env}");
    assert_eq!(part_status(&pool, pid).await, "CANCELLED");
    assert_eq!(
        part_serial_no(&pool, pid).await,
        None,
        "part 到 CANCELLED，序列号必须被释放（改造前永久泄漏）"
    );

    let note: Option<String> = sqlx::query_scalar(
        "SELECT note FROM t_part_event \
         WHERE part_id = $1 AND event_type = 'SERIAL_RELEASED'",
    )
    .bind(pid)
    .fetch_optional(&pool)
    .await
    .expect("query SERIAL_RELEASED")
    .flatten();
    let note = note.expect("应有 1 条 SERIAL_RELEASED 归档事件");
    assert!(
        note.contains(&serial),
        "归档 note 应含原序列号 {serial}，实际 {note}"
    );
}

// ===========================================================================
//  2026-10-01 review 第 1 轮 B1：派生层不得覆盖主操作（cancel 路径）
// ===========================================================================

/// 造一个「父装配件 + 单子件 + 两条批次（COMPLETED / INSPECTION）」的场景。
///
/// 形状与 B1 故障现场逐字一致：part 停在 INSPECTION，批次一条已 COMPLETED、
/// 一条还在 INSPECTION。
async fn insert_assembly_with_mixed_batches(
    pool: &PgPool,
    customer_id: i64,
    asm_serial: &str,
    part_serial: &str,
) -> (i64, i64, i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    // instance=13 独占：与文件内其它 helper 的 instance 段不重叠
    let sf = SnowflakeIdGenerator::new(1_577_836_800_000, 13);
    let asm_id = sf.next_id();
    let part_id = sf.next_id();
    let b_done = sf.next_id();
    let b_live = sf.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, serial_no, drawing_no, name, applicant_name, \
         customer_id, request_date, planned_delivery_date, status, quantity, unit_price, \
         total_price, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'DA-001', '总成', '', $3, $4, $4, 'INSPECTION', 1, 0, 0, 0, $5, NULL, $5, NULL)",
    )
    .bind(asm_id)
    .bind(asm_serial)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly");
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at, assembly_id) \
         VALUES ($1, $2, 'P-B1', 'D-001', $3, 'INSPECTION', 'P-B1', $4, $4, 1, 0, $5, $5, $6)",
    )
    .bind(part_id)
    .bind(part_serial)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(asm_id)
    .execute(pool)
    .await
    .expect("insert part");
    for (idx, (bid, st)) in [(b_done, "COMPLETED"), (b_live, "INSPECTION")]
        .into_iter()
        .enumerate()
    {
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $3, 1, $4, 0, $5, $5)",
        )
        .bind(bid)
        .bind(part_id)
        .bind(idx as i32 + 1)
        .bind(st)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert batch");
    }
    (asm_id, part_id, b_done, b_live)
}

/// **B1 回归测试**：`POST /parts/{id}/cancel` 后 part 必须停在 CANCELLED，
/// 父装配件绝不能被级联推成 COMPLETED。
///
/// 故障机制（review 第 1 轮已复现并 ROLLBACK 验证）：`PartService::cancel`
/// 先跑 `mark_part_cancelled`（**主操作**：part → CANCELLED、清 `serial_no`），
/// 紧接着 `cancel_all_active_batches_for_part` 走 status_gate 的 bulk 模式。
/// 该 part 下存在一条 **已 COMPLETED** 的批次，于是 min-progress 算出
/// `non_cancelled=[COMPLETED]` / `non_terminal=[]` → target = **COMPLETED**。
/// 改造前 `update_part_rollup` 的 WHERE 没有终态守卫，派生写直接把 CANCELLED
/// 覆盖回 COMPLETED，随后 step 3 把父装配件也推成 COMPLETED —— 而接口返回 200、
/// 事件流水记的是 `INSPECTION → CANCELLED`、界面显示「已完成」、序列号已被清空
/// 可被复用。
#[tokio::test]
async fn cancel_part_is_not_overwritten_by_rollup_completed() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (asm_id, pid, b_done, b_live) =
        insert_assembly_with_mixed_batches(&pool, fx.customer_l2_id, "ASM-B1-0001", "SN-B1-0001")
            .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/cancel"),
            Some(json!({ "reason": "客户作废" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel 200: {env}");
    assert_eq!(env["code"], 0, "信封 code 应为 0");
    assert_eq!(
        env["data"]["status"], "CANCELLED",
        "响应体应报 CANCELLED（= 主操作的结果）"
    );

    // ---- 1. part 状态：必须停在 CANCELLED（不是 COMPLETED）----
    assert_eq!(
        part_status(&pool, pid).await,
        "CANCELLED",
        "派生层不得把主操作写下的 CANCELLED 覆盖成 COMPLETED（review B1）"
    );
    assert_eq!(
        part_serial_no(&pool, pid).await,
        None,
        "cancel 走 `mark_part_cancelled` 的 `serial_no = NULL`（作废即退役，不归档）"
    );

    // ---- 2. 批次：已完成的不许被拖成 CANCELLED，活跃的被级联取消 ----
    let done_status: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(b_done)
        .fetch_one(&pool)
        .await
        .expect("read batch");
    assert_eq!(
        done_status, "COMPLETED",
        "已完成的批次是终态，级联取消的白名单必须排除它"
    );
    let live_status: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(b_live)
        .fetch_one(&pool)
        .await
        .expect("read batch");
    assert_eq!(live_status, "CANCELLED", "活跃批次应被级联取消");

    // ---- 3. 父装配件：绝不能是 COMPLETED ----
    let asm_status: String = sqlx::query_scalar("SELECT status FROM t_assembly WHERE id = $1")
        .bind(asm_id)
        .fetch_one(&pool)
        .await
        .expect("read assembly");
    assert_ne!(
        asm_status, "COMPLETED",
        "父装配件绝不能被级联推成 COMPLETED（子件是作废不是完成）"
    );
    // 它唯一的孩子已 CANCELLED → min-progress 规则算出父件 CANCELLED；
    // 这一步靠 bulk 入口的 `PartDerivation::KeepPartTerminalAsIs`
    // 「跳过 part 写、继续派生父层」实现。
    assert_eq!(
        asm_status, "CANCELLED",
        "唯一子件已作废，父装配件应被派生追平成 CANCELLED（不是留着 INSPECTION 漂着）"
    );
    // 父件进终态 → 序列号必须被释放（否则 uk_t_assembly_serial_no 永久占位）
    let asm_serial: Option<String> =
        sqlx::query_scalar("SELECT serial_no FROM t_assembly WHERE id = $1")
            .bind(asm_id)
            .fetch_one(&pool)
            .await
            .expect("read assembly serial");
    assert_eq!(
        asm_serial, None,
        "父装配件进终态后序列号必须释放（uk_t_assembly_serial_no 不含 status 条件）"
    );

    // ---- 4. 事件流水：记的是用户的主操作 ----
    let cancel_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_part_event \
         WHERE part_id = $1 AND event_type = 'CANCELLED' AND to_status = 'CANCELLED'",
    )
    .bind(pid)
    .fetch_one(&pool)
    .await
    .expect("count CANCELLED events");
    assert_eq!(
        cancel_events, 1,
        "应有且仅有 1 条 `→ CANCELLED` 事件（此前 DB 状态与事件流水互相矛盾）"
    );
}

// ===========================================================================
//  2026-10-01 review 第 1 轮 M2：出池 / 召回必须真的把「位置」清空
// ===========================================================================

/// **M2 回归测试**：`POST /prod/queue/recall` 必须把 `location` /
/// `current_holder_id` / `current_process_step_id` 一起清成 NULL。
///
/// 改造前 `mark_batch_with_status_and_meta` 的 SQL 是
/// `SET location=$4, current_holder_id=$5, current_process_step_id=$6`，
/// 传 `None` 就是**写 NULL**。收口成 status_gate 时若把 `None` 一律当成
/// 「保持原值」，PENDING 批次就会留着 `location='PRODUCTION_SHELF'` +
/// `current_holder_id=<货架>` + 陈旧 step —— UI 上「待投产」的工单显示还压在
/// 生产架上，且展示用的 `next_process_id`（由 step JOIN 派生）仍指着上一道工序。
#[tokio::test]
async fn recall_to_pending_clears_location_holder_and_step() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 5).await;
    // 池内状态：压在生产架上、挂着 holder、记着工序 step
    sqlx::query(
        "UPDATE t_part_batch \
         SET location = 'PRODUCTION_SHELF', \
             current_holder_id = $2, \
             current_process_id = $3, \
             current_process_step_id = $3 \
         WHERE id = $1",
    )
    .bind(bid)
    .bind(fx.production_shelf_id)
    .bind(fx.process_id)
    .execute(&pool)
    .await
    .expect("seed 池内状态");
    // 先自证：此刻这 3 列确实非空（否则断言「被清空」是空转）
    let (loc, holder, step): (Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT location, current_holder_id, current_process_step_id \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("read before recall");
    assert_eq!(loc.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(holder, Some(fx.production_shelf_id));
    assert_eq!(step, Some(fx.process_id));

    let version = batch_version(&pool, bid).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/recall",
            Some(json!({
                "batch_id": bid.to_string(),
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "recall: {env}");
    assert_eq!(env["data"]["batch_id"], bid.to_string(), "{env}");

    let (loc, holder, pid_col, step): (Option<String>, Option<i64>, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT location, current_holder_id, current_process_id, current_process_step_id \
             FROM t_part_batch WHERE id = $1",
        )
        .bind(bid)
        .fetch_one(&pool)
        .await
        .expect("read after recall");
    assert_eq!(loc, None, "recall 出池后 location 必须清 NULL（review M2）");
    assert_eq!(holder, None, "recall 出池后 current_holder_id 必须清 NULL");
    assert_eq!(
        pid_col, None,
        "出池后 current_process_id 必须清 NULL（池归属不变式）"
    );
    assert_eq!(
        step, None,
        "recall 出池后 current_process_step_id 必须清 NULL（review M2）"
    );
}

// ===========================================================================
//  2026-10-06：recall 放宽到「工人持有中」的批次
// ===========================================================================

/// 2026-10-06：`IN_PROCESS + location='WORKER' + holder=工人` 的批次可被召回，
/// 且 `location` / `current_holder_id` / `current_process_id` 三列一并清 NULL。
///
/// 断言结构照抄 `recall_to_pending_clears_location_holder_and_step`（先自证召回前
/// 三列非空，否则「被清空」是空转）。
#[tokio::test]
async fn recall_to_pending_allows_worker_held_batch() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let part_id = insert_part_biz(
        &pool,
        fx.customer_l2_id,
        "工人持有召回工单",
        "D-RECALL-WORKER",
        false,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap(),
        None,
    )
    .await;
    let worker_id = insert_worker(&pool, fx.work_type_id, "WT-RECALL").await;
    let bid = insert_worker_held_batch(&pool, part_id, worker_id).await;
    // 池内状态还差 current_process_id（helper 只写 location + holder），
    // 补上以便「清 NULL」这条断言非空转。
    sqlx::query("UPDATE t_part_batch SET current_process_id = $2 WHERE id = $1")
        .bind(bid)
        .bind(fx.process_id)
        .execute(&pool)
        .await
        .expect("seed current_process_id");

    // 自证：召回前这 3 列确实非空
    let (loc, holder, pid_col): (Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT location, current_holder_id, current_process_id FROM t_part_batch WHERE id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("read before recall");
    assert_eq!(loc.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(worker_id));
    assert_eq!(pid_col, Some(fx.process_id));

    let version = batch_version(&pool, bid).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/recall",
            Some(json!({
                "batch_id": bid.to_string(),
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "工人持有中的 IN_PROCESS 批次应可召回: {env}"
    );
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["batch_id"], bid.to_string(), "{env}");

    // 事后：3 列全清（工人工位容量按 location+holder 实时 COUNT，无需额外回收）
    let (loc, holder, pid_col): (Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT location, current_holder_id, current_process_id FROM t_part_batch WHERE id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("read after recall");
    assert_eq!(loc, None, "召回后 location 必须清 NULL");
    assert_eq!(holder, None, "召回后 current_holder_id 必须清 NULL");
    assert_eq!(pid_col, None, "召回后 current_process_id 必须清 NULL");

    // 工人持有列表的谓词是 `location='WORKER' AND current_holder_id=$worker_id`
    // （take_one_from_pool 的 held 容量 CTE 同谓词）⇒ 召回后该批次对工人工位
    // 立即不可见，工位容量随之回落，无需额外回收动作。
    let still_held: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_batch \
         WHERE location = 'WORKER' AND current_holder_id = $1 AND deleted_at IS NULL",
    )
    .bind(worker_id)
    .fetch_one(&pool)
    .await
    .expect("count worker-held rows");
    assert_eq!(
        still_held, 0,
        "召回后批次不得残留在工人持有列表 / 工位容量计数里"
    );
}

/// 2026-10-06：location 白名单是「生产架 + 工人」，其余 location 仍拒。
///
/// 与上面的放行用例成对，证明这条守卫是白名单而非「IN_PROCESS 一律放行」。
#[tokio::test]
async fn recall_to_pending_rejects_non_production_location() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 5).await;
    // INSPECTION_SHELF（品检架）不在白名单内
    sqlx::query(
        "UPDATE t_part_batch SET location = 'INSPECTION_SHELF', current_holder_id = $2 \
                 WHERE id = $1",
    )
    .bind(bid)
    .bind(fx.inspection_shelf_id)
    .execute(&pool)
    .await
    .expect("seed inspection shelf location");
    let version = batch_version(&pool, bid).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/recall",
            Some(json!({
                "batch_id": bid.to_string(),
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "品检架上的 IN_PROCESS 批次不应被召回: {env}"
    );
    assert_eq!(env["code"], 20103);

    // 拒绝路径不得改动任何列
    let (status, loc): (String, Option<String>) =
        sqlx::query_as("SELECT status, location FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("read after rejected recall");
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(loc.as_deref(), Some("INSPECTION_SHELF"));
}

// ===========================================================================
//  2026-10-04：工种 / 工人维度三条 list 端点的 part 侧真实字段投影
// ===========================================================================
//
// 三个端点（`GET /parts/by-work-type/{id}` / `GET /parts/pickable-by-work-type/{id}`
// / `GET /parts/by-worker/{id}`）此前共享一个根因：取行 SQL 只投影
// `p.id` / `p.serial_no` / `p.drawing_no` 三列，剩下的 `PartListItem` 字段靠**手抄
// 20 个占位值**的 `TPart { ... }` 字面量填。于是 `name` 填成图号副本（前端卡片第 1
// 行与第 2 行重复）、`is_urgent` 恒 `false`（加急 tag 永不渲染）、
// `system_delivery_date` 恒 `null`（交期 chip 永不渲染）、
// `planned_delivery_date` 恒 `1970-01-01`。
//
// 而 `pickable-by-work-type` 的 `ORDER BY p.is_urgent DESC,
// p.planned_delivery_date ASC` 排的是 **DB 真实列** ⇒ 列表已经按加急排好了，工件上
// 却看不出任何标记。
//
// 2026-10-04 起三处改为投影 `p.name` / `p.is_urgent` / `p.system_delivery_date` /
// `p.planned_delivery_date` 真实值并共用 `WorkTypeListRow` 投影 struct。本节把
// 「真实值」与「响应不含 `next_process_id`」两条不变量锁在这三个端点上。
//
// ## 断言手法
// `insert_part_biz` 造 part 时让 `name` **不等于** `drawing_no`：改造前 `name`
// 就是图号副本，两者相等时任何 `assert_eq!(name, ...)` 都测不出漂移。
// 另外 `is_urgent` / `planned_delivery_date` 刻意取**非默认值**（`false` / 当天），
// 否则「投影到真值」与「仍填占位」不可区分。

/// 插一个 part，显式带上本次要断言的 4 个 part 侧业务列，返回 part id。
async fn insert_part_biz(
    pool: &PgPool,
    customer_id: i64,
    name: &str,
    drawing_no: &str,
    is_urgent: bool,
    planned_delivery_date: chrono::NaiveDate,
    system_delivery_date: Option<chrono::NaiveDate>,
) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let part_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, customer_id, status, applicant_name, \
         request_date, planned_delivery_date, system_delivery_date, is_urgent, quantity, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 'IN_PROCESS', '', $5, $6, $7, $8, 1, 0, now(), now())",
    )
    .bind(part_id)
    .bind(name)
    .bind(drawing_no)
    .bind(customer_id)
    .bind(planned_delivery_date)
    .bind(planned_delivery_date)
    .bind(system_delivery_date)
    .bind(is_urgent)
    .execute(pool)
    .await
    .expect("insert t_part (biz fields)");
    part_id
}

/// 插一个 active 且绑 `work_type_id` 的工人（`by-work-type` 走 `t_worker` JOIN，
/// `by-worker` 以 worker_id 为过滤锚点）。
async fn insert_worker(pool: &PgPool, work_type_id: i64, code: &str) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let worker_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) VALUES ($1, $2, $3, true, $4, 0, now(), now())",
    )
    .bind(worker_id)
    .bind(code)
    .bind(format!("{code}-NAME"))
    .bind(work_type_id)
    .execute(pool)
    .await
    .expect("insert t_worker");
    worker_id
}

/// 造一条「可领取」批次：PRODUCTION_SHELF + holder=生产架 + current_process_id=工序。
/// 前两条 list 端点要求批次满足这三条 + part 未软删 + 工种↔工序映射活跃。
async fn insert_pickable_batch(pool: &PgPool, part_id: i64, shelf_id: i64, process_id: i64) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, current_process_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 3, 'IN_PROCESS', 'PRODUCTION_SHELF', $3, $4, 0, now(), now())",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(shelf_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert t_part_batch (PRODUCTION_SHELF)");
    batch_id
}

/// 造一条「工人持有中」批次：IN_PROCESS + location='WORKER' + holder=worker。
async fn insert_worker_held_batch(pool: &PgPool, part_id: i64, worker_id: i64) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 3, 'IN_PROCESS', 'WORKER', $3, 0, now(), now())",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(worker_id)
    .execute(pool)
    .await
    .expect("insert t_part_batch (WORKER-held)");
    batch_id
}

/// 打一次 list 端点并按 part id 取目标行（找不到即 panic 并打印整份信封）。
async fn list_item(app: &axum::Router, token: &str, uri: &str, part_id: i64) -> Value {
    let (s, env) = send(
        app.clone(),
        json_request("GET", uri, None::<Value>, Some(token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(env["code"], 0, "GET {uri}: {env}");
    let want = part_id.to_string();
    env["data"]["items"]
        .as_array()
        .expect("data.items")
        .iter()
        .find(|it| it["id"].as_str() == Some(want.as_str()))
        .unwrap_or_else(|| panic!("part {part_id} 不在 {uri} 的结果里: {env}"))
        .clone()
}

/// 断言一个 item 的 4 个 part 侧业务列都是 DB 真值。
///
/// 同时锁死 `next_process_id` **不出现在响应里**（`PartListItem` 根本没有该字段，
/// 序列化后不应有这个键）—— 这条不变量在 2026-10-04 之前是靠「`TPart` 字面量里
/// 写 `next_process_id: None` + `From<TPart>` 不复制它」两处巧合隐式达成的，
/// 重构后改由类型系统保证，本断言是它的线缆级守卫。
fn assert_real_part_fields(
    item: &Value,
    want_name: &str,
    want_urgent: bool,
    want_planned: &str,
    want_system: Option<&str>,
) {
    assert_eq!(
        item["name"], want_name,
        "name 必须是 t_part.name 真实值（改造前是 drawing_no 的副本）: {item}"
    );
    assert_eq!(
        item["is_urgent"], want_urgent,
        "is_urgent 必须是 t_part.is_urgent 真实值（改造前恒 false）: {item}"
    );
    assert_eq!(
        item["planned_delivery_date"], want_planned,
        "planned_delivery_date 必须是真实值（改造前恒 1970-01-01）: {item}"
    );
    match want_system {
        Some(s) => assert_eq!(
            item["system_delivery_date"], s,
            "system_delivery_date 必须是 t_part 的真实值: {item}"
        ),
        None => assert!(
            item["system_delivery_date"].is_null(),
            "system_delivery_date 为 NULL 的 part 必须序列化成 null: {item}"
        ),
    }
    assert!(
        item.get("next_process_id").is_none(),
        "列表响应恒不含 next_process_id（PartListItem 无该字段）: {item}"
    );
}

/// `GET /parts/by-work-type/{id}`：part 侧 4 列投影真实值。
#[tokio::test]
async fn by_work_type_projects_real_part_business_fields() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let part_id = insert_part_biz(
        &pool,
        fx.customer_l1_id,
        "急件工单名",
        "D-BIZ-WT",
        true,
        chrono::NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(),
        Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 30).unwrap()),
    )
    .await;
    let worker_id = insert_worker(&pool, fx.work_type_id, "WT-BIZ").await;
    insert_worker_held_batch(&pool, part_id, worker_id).await;

    let item = list_item(
        &app,
        &token,
        &format!("/parts/by-work-type/{}", fx.work_type_id),
        part_id,
    )
    .await;
    assert_real_part_fields(&item, "急件工单名", true, "2026-12-31", Some("2026-11-30"));
    assert_ne!(
        item["name"], item["drawing_no"],
        "前提自证：name 与 drawing_no 必须不同（相等时测不出「name 填成图号」的漂移）: {item}"
    );
    // 本端点的行不是「批次锚点」语义（口径见 vo/part.rs 字段 doc），仍是 null。
    assert!(
        item["batch_id"].is_null() && item["batch_version"].is_null(),
        "by-work-type 不填批次锚点: {item}"
    );
}

/// `GET /parts/pickable-by-work-type/{id}`：part 侧 4 列投影真实值。
#[tokio::test]
async fn pickable_by_work_type_projects_real_part_business_fields() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let part_id = insert_part_biz(
        &pool,
        fx.customer_l1_id,
        "可领急件名",
        "D-BIZ-PICK",
        true,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap(),
        None,
    )
    .await;
    insert_pickable_batch(&pool, part_id, fx.production_shelf_id, fx.process_id).await;

    let item = list_item(
        &app,
        &token,
        &format!("/parts/pickable-by-work-type/{}", fx.work_type_id),
        part_id,
    )
    .await;
    // system_delivery_date 为 NULL ⇒ 序列化成 null（不是 1970-01-01）
    assert_real_part_fields(&item, "可领急件名", true, "2026-10-20", None);
}

/// `GET /parts/by-worker/{id}`：part 侧 4 列投影真实值。
#[tokio::test]
async fn by_worker_projects_real_part_business_fields() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let part_id = insert_part_biz(
        &pool,
        fx.customer_l1_id,
        "持有中工单名",
        "D-BIZ-WK",
        true,
        chrono::NaiveDate::from_ymd_opt(2027, 1, 15).unwrap(),
        Some(chrono::NaiveDate::from_ymd_opt(2026, 12, 20).unwrap()),
    )
    .await;
    let worker_id = insert_worker(&pool, fx.work_type_id, "WK-BIZ").await;
    insert_worker_held_batch(&pool, part_id, worker_id).await;

    let item = list_item(
        &app,
        &token,
        &format!("/parts/by-worker/{worker_id}"),
        part_id,
    )
    .await;
    assert_real_part_fields(
        &item,
        "持有中工单名",
        true,
        "2027-01-15",
        Some("2026-12-20"),
    );
    // 本端点**不填**链四字段的前提：无链 ⇒ 保守默认 NONE / "0" / null / null
    assert_eq!(item["chain_state"], "NONE", "无链批次应落 NONE: {item}");
    assert_eq!(item["chain_next_process_id"].as_str(), Some("0"), "{item}");
}

/// 非加急 + 有 system_delivery_date 的组合：`is_urgent` 不得被反向填成 true。
///
/// 上一节三例全走 `is_urgent = true`，只锁住「true 能穿透」；本例锁住另一半：false
/// 是**真值**（与「未投影而恒 false」不可区分，但至少保证没有恒 true 的镜像 bug）。
#[tokio::test]
async fn by_work_type_keeps_non_urgent_and_real_system_delivery_date() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let part_id = insert_part_biz(
        &pool,
        fx.customer_l1_id,
        "不急工单",
        "D-BIZ-CALM",
        false,
        chrono::NaiveDate::from_ymd_opt(2027, 3, 1).unwrap(),
        Some(chrono::NaiveDate::from_ymd_opt(2027, 2, 1).unwrap()),
    )
    .await;
    let worker_id = insert_worker(&pool, fx.work_type_id, "WT-CALM").await;
    insert_worker_held_batch(&pool, part_id, worker_id).await;

    let item = list_item(
        &app,
        &token,
        &format!("/parts/by-work-type/{}", fx.work_type_id),
        part_id,
    )
    .await;
    assert_real_part_fields(&item, "不急工单", false, "2027-03-01", Some("2027-02-01"));
}
