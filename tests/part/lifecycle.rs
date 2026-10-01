//! part 域 Phase 1（2026-09-13）生命周期集成测试：1.1/1.2/1.3/1.4/1.7 端点。
//!
//! 覆盖：
//!   - place-on-shelf: PENDING → IN_PROCESS（happy + RBAC + 状态机拒绝 + shelf↔process 校验）
//!   - recall-to-pending: ON_SHELF/PROGRAMMING → PENDING（happy + 状态机拒绝）
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
//! 移除测试（PROGRAMMING 状态废弃进入路径；详见 `part/statemachine.rs::can_transition_to`
//! 与 `docs/api/parts/lifecycle.md`）。
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
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/place-on-shelf"),
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
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/place-on-shelf"),
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
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/place-on-shelf"),
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
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/place-on-shelf"),
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
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 5).await;
    // 写入 location=PRODUCTION_SHELF（recall-to-pending 要求）
    sqlx::query("UPDATE t_part_batch SET location = 'PRODUCTION_SHELF' WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .expect("set location");
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/recall-to-pending"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "recall: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "PENDING");
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
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/release-from-programming"),
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
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/release-from-programming"),
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
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "pass": true,
        "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
        "batch_id": bid.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/scan-inspect"),
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
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 5).await;
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
        "batch_id": bid.to_string(),
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/scan-inspect"),
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
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "DELIVERED", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "pass": true,
        "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
        "batch_id": bid.to_string(),
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/scan-inspect"),
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
        json_request("POST", "/parts/scan/deliver-part", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "无效工牌应拒绝: {env}");
    assert_eq!(env["code"], 20201);
}

// ===========================================================================
//  待编程一览新过滤规则测试（2026-09-29 改造）
//
//  覆盖：
//   - list_pending_programming_includes_parts_with_cnc_step_in_chain
//       链上含 CNC step 的 part 出现
//   - list_pending_programming_includes_parts_on_cnc_shelf_without_cnc_step_in_chain
//       chain 无 CNC 但 batch.holder 是 CNC 货架的 part 出现
//   - list_pending_programming_has_cnc_program_tab_filter
//       has_cnc_program=true/false 过滤正确
//   - list_pending_programming_excludes_completed_or_cancelled
//       验证状态白名单（PENDING/IN_PROCESS/PROGRAMMING）
//
//  共用 helper：
//   - `seed_cnc_process`  —— 插入一个新 CNC 工序（is_cnc=TRUE，区别于 fx.process_id）
//   - `seed_cnc_shelf`    —— 插入一个新 CNC 货架 + t_shelf_process 映射
//   - `attach_chain_cnc_step` —— 在 part 已绑的 chain 上追加 CNC step
//   - `seed_g_code_file`   —— 插一个 t_part_file.kind='G_CODE' 行（part 已上传 G_CODE）
// ===========================================================================

/// 插入一个新 CNC 工序（`is_cnc=TRUE`）。fixture 的 `fx.process_id` 是 INHOUSE
/// （FX-PROC-A，is_cnc=FALSE），不能当 CNC step 用。
async fn seed_cnc_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         is_cnc, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed cnc process");
    id
}

/// 插入一个 CNC 货架（zone=PRODUCTION，is_active=true）+ 绑到指定 CNC process。
/// 返回 shelf_id。
async fn seed_cnc_shelf(pool: &PgPool, code: &str, name: &str, cnc_process_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let shelf_id = snowflake.next_id();
    let sp_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'PRODUCTION', true, 0, 0, $4, $4)",
    )
    .bind(shelf_id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed cnc shelf");
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(sp_id)
    .bind(shelf_id)
    .bind(cnc_process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed cnc shelf_process mapping");
    shelf_id
}

/// 在 part 已绑的 chain 上追加 CNC step（`sort_order` 由 caller 指定）。
/// 要求 part 此前已通过 `create_chain_for_part` 建链。
async fn attach_chain_cnc_step(pool: &PgPool, chain_id: i64, cnc_process_id: i64, sort_order: i32) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let step_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 30, 0, $5, 0, $5, 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(sort_order)
    .bind(cnc_process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("attach chain cnc step");
}

/// 为指定 part 插一个 `t_part_file.kind='G_CODE'` 行（模拟已上传数控程序）。
async fn seed_g_code_file(pool: &PgPool, part_id: i64, owner_user_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    let object_key = format!("uploads/part/{part_id}/G_CODE/test_{id}.nc");
    sqlx::query(
        "INSERT INTO t_part_file (id, part_id, kind, file_type, object_key, \
         original_filename, file_size, content_type, upload_status, content_sha256, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'G_CODE', 'NC', $3, $4, 1024, 'text/plain', 'CONFIRMED', \
         'aabbccdd' || repeat('0', 56), $5, $6, $5, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(object_key)
    .bind(format!("test_{id}.nc"))
    .bind(now)
    .bind(owner_user_id)
    .execute(pool)
    .await
    .expect("seed g_code file");
    id
}

#[tokio::test]
async fn list_pending_programming_includes_parts_with_cnc_step_in_chain() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // part 状态 = PENDING，链上有 CNC step（is_cnc=TRUE）
    let (pid, _bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 1).await;
    let cnc_proc = seed_cnc_process(&pool, "CNC-TEST", "测试 CNC 工序").await;
    let chain_id = create_chain_for_part(&pool, pid).await;
    attach_chain_cnc_step(&pool, chain_id, cnc_proc, 1).await;
    // 调用
    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/parts/pending-programming",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list 200: {env}");
    let items = env["data"]["items"].as_array().expect("items array");
    let pid_str = pid.to_string();
    assert!(
        items
            .iter()
            .any(|it| it["id"].as_str() == Some(pid_str.as_str())),
        "链上 CNC step 的 part 应出现在列表中: {env}"
    );
}

#[tokio::test]
async fn list_pending_programming_includes_parts_on_cnc_shelf_without_cnc_step_in_chain() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let cnc_proc = seed_cnc_process(&pool, "CNC-TEST2", "CNC-2").await;
    let cnc_shelf = seed_cnc_shelf(&pool, "CNC-SHELF-TEST", "CNC 货架", cnc_proc).await;
    // part 状态 = IN_PROCESS，链上无 CNC step，但 batch 在 CNC 货架
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 1).await;
    let chain_id = create_chain_for_part(&pool, pid).await;
    // chain 上挂 fx.process_id（INHOUSE，非 CNC）—— 验证条件 A 不命中
    let _step = create_step(&pool, chain_id, fx.process_id, 1).await;
    // 把 batch 移到 CNC 货架 —— 命中条件 B
    sqlx::query(
        "UPDATE t_part_batch SET location = 'PRODUCTION_SHELF', current_holder_id = $1, \
         version = version + 1 WHERE id = $2",
    )
    .bind(cnc_shelf)
    .bind(bid)
    .execute(&pool)
    .await
    .expect("move batch to cnc shelf");
    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/parts/pending-programming",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list 200: {env}");
    let items = env["data"]["items"].as_array().expect("items array");
    let pid_str = pid.to_string();
    assert!(
        items
            .iter()
            .any(|it| it["id"].as_str() == Some(pid_str.as_str())),
        "批次位于 CNC 货架的 part 应出现在列表中（条件 B）: {env}"
    );
}

#[tokio::test]
async fn list_pending_programming_has_cnc_program_tab_filter() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let cnc_proc = seed_cnc_process(&pool, "CNC-TEST3", "CNC-3").await;
    // part A：链上有 CNC step，但**未**上传 G_CODE
    let (pid_a, _bid_a) = insert_part_with_batch(&pool, "A", fx.customer_l2_id, "PENDING", 1).await;
    let chain_a = create_chain_for_part(&pool, pid_a).await;
    attach_chain_cnc_step(&pool, chain_a, cnc_proc, 1).await;
    // part B：链上有 CNC step，且**已**上传 G_CODE
    let (pid_b, _bid_b) = insert_part_with_batch(&pool, "B", fx.customer_l2_id, "PENDING", 1).await;
    let chain_b = create_chain_for_part(&pool, pid_b).await;
    attach_chain_cnc_step(&pool, chain_b, cnc_proc, 1).await;
    seed_g_code_file(&pool, pid_b, fx.manager_user_id).await;
    // Tab = false（待编程）：A 应出现，B 不应出现
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            "/parts/pending-programming?has_cnc_program=false",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "tab=false: {env}");
    let items = env["data"]["items"].as_array().expect("items array");
    let ids: Vec<String> = items
        .iter()
        .map(|it| it["id"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        ids.iter().any(|s| s == &pid_a.to_string()),
        "未上传 G_CODE 的 A 应出现在 tab=false: {env}"
    );
    assert!(
        !ids.iter().any(|s| s == &pid_b.to_string()),
        "已上传 G_CODE 的 B 不应出现在 tab=false: {env}"
    );
    // Tab = true（已编程）：B 应出现，A 不应出现
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            "/parts/pending-programming?has_cnc_program=true",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "tab=true: {env2}");
    let items2 = env2["data"]["items"].as_array().expect("items array");
    let ids2: Vec<String> = items2
        .iter()
        .map(|it| it["id"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        ids2.iter().any(|s| s == &pid_b.to_string()),
        "已上传 G_CODE 的 B 应出现在 tab=true: {env2}"
    );
    assert!(
        !ids2.iter().any(|s| s == &pid_a.to_string()),
        "未上传 G_CODE 的 A 不应出现在 tab=true: {env2}"
    );
}

#[tokio::test]
async fn list_pending_programming_excludes_completed_or_cancelled() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let cnc_proc = seed_cnc_process(&pool, "CNC-TEST4", "CNC-4").await;
    // 4 个 part：COMPLETED / CANCELLED 不应出现，PENDING / IN_PROCESS 应出现
    let (pid_done, _) =
        insert_part_with_batch(&pool, "DONE", fx.customer_l2_id, "COMPLETED", 1).await;
    let (pid_cancel, _) =
        insert_part_with_batch(&pool, "CANCEL", fx.customer_l2_id, "CANCELLED", 1).await;
    let (pid_pend, _) =
        insert_part_with_batch(&pool, "PEND", fx.customer_l2_id, "PENDING", 1).await;
    let (pid_proc, _) =
        insert_part_with_batch(&pool, "PROC", fx.customer_l2_id, "IN_PROCESS", 1).await;
    for pid in [pid_done, pid_cancel, pid_pend, pid_proc] {
        let chain_id = create_chain_for_part(&pool, pid).await;
        attach_chain_cnc_step(&pool, chain_id, cnc_proc, 1).await;
    }
    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/parts/pending-programming",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list 200: {env}");
    let items = env["data"]["items"].as_array().expect("items array");
    let ids: Vec<String> = items
        .iter()
        .map(|it| it["id"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        ids.iter().any(|s| s == &pid_pend.to_string()),
        "PENDING 应出现: {env}"
    );
    assert!(
        ids.iter().any(|s| s == &pid_proc.to_string()),
        "IN_PROCESS 应出现: {env}"
    );
    assert!(
        !ids.iter().any(|s| s == &pid_done.to_string()),
        "COMPLETED 不应出现: {env}"
    );
    assert!(
        !ids.iter().any(|s| s == &pid_cancel.to_string()),
        "CANCELLED 不应出现: {env}"
    );
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
            &format!("/parts/{pid}/complete"),
            Some(json!({ "batch_id": b1.to_string(), "version": v1 })),
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
            &format!("/parts/{pid}/complete"),
            Some(json!({ "batch_id": b2.to_string(), "version": v2 })),
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
/// （`POST /parts/{id}/batches/{batch_id}/cancel`）只翻批次、调 rollup，
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
            &format!("/parts/{pid}/batches/{b1}/cancel"),
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
            &format!("/parts/{pid}/batches/{b2}/cancel"),
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

/// **M2 回归测试**：`recall-to-pending` 必须把 `location` /
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
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 5).await;
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
            &format!("/parts/{pid}/recall-to-pending"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "recall: {env}");
    assert_eq!(env["data"]["status"], "PENDING");

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
