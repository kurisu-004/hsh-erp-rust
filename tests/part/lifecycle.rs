//! part 域 Phase 1（2026-09-13）生命周期集成测试：1.1/1.2/1.3/1.4/1.7 端点。
//!
//! 覆盖：
//!   - place-on-shelf: PENDING → IN_PROCESS（happy + RBAC + 状态机拒绝 + shelf↔process 校验）
//!   - recall-to-pending: ON_SHELF/PROGRAMMING → PENDING（happy + 状态机拒绝）
//!   - release-from-programming: PROGRAMMING → IN_PROCESS（happy + RBAC）
//!   - send-to-outsource: PENDING → OUTSOURCE
//!   - receive-from-outsource: OUTSOURCE → IN_PROCESS
//!   - receive-from-outsource-to-inspection: OUTSOURCE → INSPECTION
//!   - complete-repair: REPAIRING → IN_PROCESS / INSPECTION
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
    assert_eq!(env["data"]["status"], "REPAIRING");
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
