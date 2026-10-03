//! worker_pool 域端到端集成测试（Task 10 / plan §11）
//!
//! 场景清单（回归场景持续追加，清单不承诺与文件内测试函数一一对应；某条
//! 能力的完整覆盖以函数名为准）：
//!   1. worker_scan INSPECTED → 自动 refill
//!   2. worker_scan RETURNED → 自动 refill；RETURNED 推进 `current_process_id`
//!      （2026-09-30 回归：批次落进**下一道**工序池而非原池）；
//!      另有 2d（2026-10-04 回归）：RETURNED 在 part **无工艺链**时也必须成功 ——
//!      `t_part.process_chain_id` 可空，按 `i64` 解码会把整个 RETURNED 打成 500，
//!      而 fixture 必须能造出「无链」形态，否则该 500 恒被免疫屏障挡住
//!   3. refill_when_pool_empty_returns_empty
//!   4. refill_caps_at_max_held_batches
//!   5. concurrent_refill_no_double_pick       [`#[ignore]`：需 app-level 并发基建]
//!   6. refill_respects_shelf_scope            （同 7 节外）
//!   7. refill_skips_concurrently_modified_batch [`#[ignore]`：需并发模拟基建]
//!   8. worker_scan_shelf_scope_violation_403
//!   9. take_updates_t_part_holder
//!  10. take_does_not_update_placed_at
//!  11. events_persisted_to_t_part_event
//!  12. admin_refill_endpoint_works
//!  13. admin_remove_returns_batch_to_pool
//!  14. refill_failure_rolls_back_worker_scan   [`#[ignore]`：DB 故障注入缺基建]
//!  15. max_held_null_returns_error
//!  16. worker_no_work_type_returns_error
//!
//! ## 串行化
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex / `--test-threads=1` 双保险。
//!
//! ## clippy allow
//! 2026-09-16 PR-3：fixture helper（`insert_pool_part` / `insert_worker_held_part` /
//!  `insert_work_type` / `insert_worker` / `insert_customer_l2` / `insert_l2_customer`）
//! 全部走 `pool_snowflake().lock()` 拿 guard 跨多个 .await SQL，模式与 common/
//! 一致，豁免 `await_holding_lock`。`unused_imports` 豁免是因为 `use
//! SnowflakeIdGenerator` 在文件顶层未直接使用（仅作为 `pool_snowflake()` 返回
//! 类型签名引用）。
//! 2026-09-23 PR13 Phase D：`#![allow]` 已在 tests/production/mod.rs 集中豁免，
//! 本文件移除。
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! 通用 `login_manager` helper，统一走 `use hsh_erp_test_support::{...}` +
//! `bootstrap_as_manager()` + `load_production_fixture(&pool)`。保留：
//! - `login_shelf_account`：worker_pool 独享（要求 scope 限定到 production shelves，
//!   fixture 的 fx_part_shelf scope=inspection_shelf 不通用）
//! - `login_manager_with_username`：worker_pool 独享（多次以不同 username 登入
//!   触发不同 OCC / 审计场景；token 重登不删 fixture 的 fx_part_manager 用户，
//!   故需要动态 create user + MANAGER role + login）
//! - `insert_work_type` / `insert_worker` / `insert_customer_l2` / `insert_l2_customer` /
//!   `insert_pool_part` / `insert_worker_held_part` / `count_held_by_worker`：
//!   worker_pool 独享的 raw SQL 构造（绕开业务 API，按需造多对 pool / held 件）；
//!   跨 binary 不重用，保留为本地 fn。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    ProductionFixture, json_request, load_production_fixture, login_token, pool_snowflake, send,
    test_app, test_pool, test_state,
};

// ===========================================================================
//  动态 fixture helpers（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::{insert_user_with_password / add_role /
//  seed_process / link_work_type_to_process / link_shelf_to_process / insert_shelf}`
//  引入 6 helper，因 fixtures.rs 本轮被删，复制到本地（同形 sqlx::query 直插）。
// ===========================================================================

/// 插一个 `is_active=true` 的 `t_user` 行（bcrypt 哈希现场生成）。
async fn insert_user_with_password(pool: &PgPool, username: &str, plain_password: &str) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
    )
    .bind(id)
    .bind(username.to_lowercase())
    .bind(hash)
    .bind(username)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user");
    id
}

/// 插一个 `t_user_role` 行（user_id + role + scope）。
async fn add_role(
    pool: &PgPool,
    user_id: i64,
    role: &str,
    scope_type: Option<&str>,
    scope_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(user_id)
    .bind(role)
    .bind(scope_type)
    .bind(scope_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");
    id
}

/// 插一个 INHOUSE 类别的 `t_process` 工序。
async fn seed_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

/// 直插一条 **active** 的 `t_work_type_process` 映射（`deleted_at` 留默认 NULL）。
async fn link_work_type_to_process(pool: &PgPool, wt_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(id)
    .bind(wt_id)
    .bind(p_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_work_type_process");
}

/// 直插一条 **active** 的 `t_shelf_process` 映射（`deleted_at` 留默认 NULL）。
async fn link_shelf_to_process(pool: &PgPool, s_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(id)
    .bind(s_id)
    .bind(p_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
}

/// 插一个 t_shelf 行（code / name / zone）。
async fn insert_shelf(pool: &PgPool, code: &str, name: &str, zone: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(zone)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 production fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// SHELF_ACCOUNT user：scope 限制在指定 shelves（不传 → wildcard 全开放）。
///
/// worker_pool 独享：每个测试需要不同 scope 限定到 production shelves，
/// fixture 的 fx_part_shelf scope=inspection_shelf 不通用，故保留本地 helper。
async fn login_shelf_account(
    pool: PgPool,
    username: &str,
    shelves: &[i64],
) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    for sid in shelves {
        add_role(&pool, uid, "SHELF_ACCOUNT", Some("shelf"), Some(*sid)).await;
    }
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let req = json_request(
        "POST",
        "/iam/login",
        Some(json!({"username": username, "password": "changeme"})),
        None,
    );
    let (_, env) = send(app, req).await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token, pool)
}

/// MANAGER user：以新 username 登入 + MANAGER role（不影响 fixture 的 fx_part_manager）。
///
/// worker_pool 独享：每个测试常需要多次以不同 username 登入触发不同 OCC / 审计
/// 场景（如 admin3 → admin3b 模拟并发 OCC）。`bootstrap_as_manager` 的 token
/// 是 fx_part_manager 单 token，不支持多身份切换；保留为本地 helper。
async fn login_manager_with_username(pool: &PgPool, username: &str) -> (axum::Router, String) {
    let uid = insert_user_with_password(pool, username, "changeme").await;
    add_role(pool, uid, "MANAGER", None, None).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let req = json_request(
        "POST",
        "/iam/login",
        Some(json!({"username": username, "password": "changeme"})),
        None,
    );
    let (_, env) = send(app, req).await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token)
}

// ===========================================================================
//  worker-pool fixture helpers（worker_pool 独享，跨 binary 不迁移）
// ===========================================================================

async fn insert_work_type(pool: &PgPool, code: &str, name: &str, max_held: Option<i32>) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_work_type (id, code, name, sort_order, version, \
         max_held_batches, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $5, $5)",
        id,
        code,
        name,
        max_held,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_work_type");
    id
}

async fn insert_worker(
    pool: &PgPool,
    badge_code: &str,
    name: &str,
    work_type_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, true, $4, 0, $5, $5)",
        id,
        badge_code,
        name,
        work_type_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

async fn insert_customer_l2(pool: &PgPool, name: &str) -> i64 {
    // 2026-09-24 PR13 Phase H：插 L2 叶子客户（parent_id=fx_part_customer_l1_id=10，
    // serial_prefix=NULL），不再插根客户。原版插根客户（parent_id=NULL + serial_prefix='P'
    // 之类），与 part fixture 的 CUSTOMER_L1_ID=10 (prefix='P') 撞
    // `uq_t_customer_root_prefix`（parent_id IS NULL + serial_prefix 全局活跃唯一）。
    //
    // 用 `sqlx::query`（runtime）而非 `query!`：SQL 与原版 query! 不同（parent_id 改
    // 为 fixture L1），改 query! 会触发 sqlx::prepare 重新生成 .sqlx cache（会清掉同
    // worktree 其它测试文件仍在用的离线 metadata）。
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let l2_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, NULL, 0, $4, $4)",
    )
    .bind(l2_id)
    .bind(name)
    // PartFixture::CUSTOMER_L1_ID 字面值（来自 part.sql 第 46 行）
    .bind(9_000_000_000_000_000_010_i64)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2 customer under fixture L1");
    l2_id
}

/// 进程级共享雪花 ID 生成器（2026-09-11 PR-B2 后续修复）。
///
/// 多个集成测试在同一毫秒内连发 `insert_pool_part` 等 helper —— 每次独立构
/// 造 `SnowflakeIdGenerator::new(...)` 会让 sequence=0 在同一毫秒内拿到相同
/// id（23505 pkey 冲突）。改用 `OnceLock` 共享一个生成器，sequence 自增避
/// 免重复。
/// 插一个 IN_PROCESS+PRODUCTION_SHELF 工单 + 批次 + placed_at。
/// 返回 (part_id, batch_id)。
///
/// 2026-09-11 修复：批量插入时多次独立构造 `SnowflakeIdGenerator` 会在同一毫
/// 秒内产生重复 id（23505 pkey 冲突）。改用进程级共享生成器 `pool_snowflake()`
/// —— 内部 `next_id()` 自带 sequence 递增，避免重复。
async fn insert_pool_part(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
    shelf_id: i64,
    process_id: i64,
    quantity: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let now = now_naive();
    let today = now.date();
    let part_id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    // 2026-09-16 PR-2（migration 027）：t_part 删 `location` / `current_holder_id` /
    // `placed_at` 等批次依附列（位置/持有人真相源改在 t_part_batch 同名列）；
    // INSERT 列名与 VALUES 占位符同步移除：'PRODUCTION_SHELF' / $5（shelf_id）/
    // $3（now 用作 placed_at）。
    // 2026-09-16 PR-3 批次 step 化：worker_pool 候选池要求 part 已绑定工艺链
    // 且 batch 持有 current_process_step_id。helper 多走两步：建链 → 建 step。
    let chain_id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    sqlx::query!(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 0, $3, 0, $3, 0)",
        chain_id,
        format!("chain-{serial_no}"),
        now,
    )
    .execute(pool)
    .await
    .expect("insert chain");
    let step_id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    sqlx::query!(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 30, 0, $4, 0, $4, 0)",
        step_id,
        chain_id,
        process_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert chain step");
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, \
         request_date, planned_delivery_date, system_delivery_date, status, \
         is_urgent, next_process_id, customer_id, \
         quantity, version, created_at, updated_at, process_chain_id) \
         VALUES ($1, $2, 'pool-item', 'D-POOL', $2, $4, $4, $4, 'IN_PROCESS', \
         false, $3, $5, $6, 0, $7, $7, $8)",
        part_id,
        serial_no,
        process_id,
        today,
        customer_id,
        quantity,
        now,
        chain_id,
    )
    .execute(pool)
    .await
    .expect("insert t_part");
    let batch_id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    // 2026-09-16 PR-3 批次 step 化：删 `next_process_id` / `placed_at` 列；
    // 改为 `current_process_step_id`。
    // 2026-09-30：候选池归属改按 `current_process_id` 普通过滤
    // （不再 JOIN t_process_chain_step），helper 必须同时写这两列。
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, current_process_id, current_process_step_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, 'IN_PROCESS', 'PRODUCTION_SHELF', $4, $5, $6, 0, $7, $7)",
        batch_id,
        part_id,
        quantity,
        shelf_id,
        process_id,
        step_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    (part_id, batch_id)
}

/// worker-scan happy path 准备：worker 当前持有 1 个 IN_PROCESS+WORKER 批次。
///
/// 入参：
/// - `worker_id`：worker.id
/// - `next_process_id`：batch.next_process_id（必填 RETURNED）
/// - `with_chain`：part 是否绑定工艺链。`false` ⇒ `t_part.process_chain_id` 留 NULL
///   （手写工单的常态）
///
/// 返回 (part_id, batch_id, step_id)。`step_id` 是批次 `current_process_step_id` 的
/// 入参值，恒非 NULL —— 两种 `with_chain` 都建 chain/step 行，`with_chain` 只控制 part
/// 是否**绑**上它（`with_chain=false` 时 step 行是「孤儿」，与真实数据里「part 无链、
/// 批次仍带 step 定位」同形；也让调用方能断言 RETURNED 后该值被保留）。
///
/// 2026-09-16 PR-3 fix：复用同一 `snowflake` guard 生成所有 id，不要再
/// `pool_snowflake().lock()` 第二次——`std::sync::Mutex` 非递归，
/// 同线程二次 lock 会永久 hang（PR-3 step3 之前无此问题）。
///
/// 2026-10-04：fixture 必须支持「不绑链」形态。worker-scan 全部用例若都让 fixture
/// 建链并把 `process_chain_id` 绑回 part，`t_part.process_chain_id` 在用例里就恒非
/// NULL，而 `worker_scan.rs` 把该列按 `i64` 解码（该列可空 ⇒ `unexpected null` 会把
/// 整个 RETURNED 打成 500）这件事就恒测不出来。手写工单（无工艺链，工单域常态）
/// 这条主路径必须有覆盖。回归见 `worker_scan_returned_without_process_chain_succeeds`。
async fn insert_worker_held_part(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
    worker_id: i64,
    next_process_id: i64,
    quantity: i32,
    with_chain: bool,
) -> (i64, i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let now = now_naive();
    let today = now.date();
    let part_id = snowflake.next_id();
    // 2026-09-16 PR-3 批次 step 化：worker-pool 场景需要 process_chain + step
    let chain_id = snowflake.next_id();
    sqlx::query!(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 0, $3, 0, $3, 0)",
        chain_id,
        format!("chain-{serial_no}"),
        now,
    )
    .execute(pool)
    .await
    .expect("insert chain");
    let step_id = snowflake.next_id();
    sqlx::query!(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 30, 0, $4, 0, $4, 0)",
        step_id,
        chain_id,
        next_process_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert chain step");
    // 2026-10-04：`t_part.process_chain_id` 可空（列 COMMENT「NULL = 未制定工艺链」），
    // 故绑定 `Option<i64>` —— `with_chain=false` 时该列保持 NULL。
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, \
         request_date, planned_delivery_date, system_delivery_date, status, \
         is_urgent, next_process_id, customer_id, \
         quantity, version, created_at, updated_at, process_chain_id) \
         VALUES ($1, $2, 'held-item', 'D-HELD', $2, $4, $4, $4, 'IN_PROCESS', \
         false, $3, $5, $6, 0, $7, $7, $8)",
        part_id,
        serial_no,
        next_process_id,
        today,
        customer_id,
        quantity,
        now,
        with_chain.then_some(chain_id),
    )
    .execute(pool)
    .await
    .expect("insert held t_part");
    // 2026-09-16 PR-3 fix：复用 `snowflake` guard 而非 `pool_snowflake().lock()` 第二次。
    let batch_id = snowflake.next_id();
    // 2026-09-16 PR-2（migration 027）：t_part_batch 删 `has_been_repaired`；INSERT
    // 列名与 VALUES 占位符同步移除 `false` 字面量。
    // 2026-09-30：同 insert_pool_batch —— 补 `current_process_id`
    // （RETURNED 归还货架后要能落回原工序候选池）
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, current_process_id, current_process_step_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, 'IN_PROCESS', 'WORKER', $4, $5, $6, 0, $7, $7)",
        batch_id,
        part_id,
        quantity,
        worker_id,
        next_process_id,
        step_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert held t_part_batch");
    (part_id, batch_id, step_id)
}

/// 把 part_id 给定批次标为 worker 持有（针对 pool→worker 流转后的批次）。
async fn count_held_by_worker(pool: &PgPool, worker_id: i64) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!"
        FROM t_part_batch
        WHERE current_holder_id = $1 AND location = 'WORKER' AND deleted_at IS NULL"#,
        worker_id,
    )
    .fetch_one(pool)
    .await
    .expect("count held")
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: worker-scan INSPECTED → 自动 refill
#[tokio::test]
async fn worker_scan_inspected_triggers_refill() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL").await;
    let proc = seed_process(&pool, "PROC-A", "工序A").await;
    let wt = insert_work_type(&pool, "WT-A", "工种A", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-A", "PROD-A", "PRODUCTION").await;
    let insp_shelf = insert_shelf(&pool, "INSP-A", "INSP-A", "INSPECTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC001", "工1", Some(wt)).await;
    // worker 当前持 1 件；池里 1 件待 refill
    let (held_part, _held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-001", worker, proc, 1, true).await;
    let (_pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-001", prod_shelf, proc, 1).await;

    let (app, token, pool) =
        login_shelf_account(pool.clone(), "user1", &[prod_shelf, insp_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "H-001",
                "badge_code": "BC001",
                "event_type": "INSPECTED",
                "shelf_id": prod_shelf.to_string(),
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan INSPECTED: {env}");
    assert_eq!(env["code"], 0);
    // scan 出参：worker_id / part_id / event_type 都在 scan 字段里
    assert_eq!(env["data"]["scan"]["worker_id"], worker.to_string());
    assert_eq!(env["data"]["scan"]["part_id"], held_part.to_string());
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_INSPECTED");
    // refill 出参：从池里抢到 1 件 + pool_empty=false
    let taken = env["data"]["refill"]["taken"]
        .as_array()
        .expect("refill.taken");
    assert_eq!(taken.len(), 1, "refill 应抢到 1 件: {env}");
    assert_eq!(env["data"]["refill"]["pool_empty"], false);

    // 验证 worker 持有数 = 1（放回 1 + 抢到 1）
    let held = count_held_by_worker(&pool, worker).await;
    assert_eq!(held, 1, "worker 应持有 1 件（refill 后）");
}

/// 场景 2: worker-scan RETURNED → 自动 refill
#[tokio::test]
async fn worker_scan_returned_triggers_refill() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2").await;
    let proc = seed_process(&pool, "PROC-B", "工序B").await;
    let wt = insert_work_type(&pool, "WT-B", "工种B", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-B", "PROD-B", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC002", "工2", Some(wt)).await;
    let (_held_part, _held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-002", worker, proc, 1, true).await;
    let (_pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-002", prod_shelf, proc, 1).await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "H-002",
                "badge_code": "BC002",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_RETURNED");
    let taken = env["data"]["refill"]["taken"]
        .as_array()
        .expect("refill.taken");
    // REFILL 抢满 max=5；池里放回 1 件（H-002）+ 原 1 件 → refill 抢 2 件
    assert_eq!(
        taken.len(),
        2,
        "refill 应抢到 2 件（returned 1 + pool 1）: {env}"
    );
}

/// 场景 2b（2026-09-30 回归测试）: worker-scan RETURNED 推进工序
/// → 批次落进**下一道**工序的候选池，而不是落回原工序池。
///
/// 背景：`mark_batch_returned` 此前既不写 `current_process_id` 也不写
/// `current_process_step_id`，而 RETURNED 是全仓唯一的**工序推进**路径 ——
/// 工人在 PROC-B 完工、扫 RETURNED 传 `next_process_id=PROC-C`，批次归还货架后
/// 仍带 `current_process_id=PROC-B` → 落回 **PROC-B** 池。这正是 migration 004
/// 确立的「唯一权威依据」在主干流程上说谎。
///
/// 本测试直接打用户报告的那个症状面：扫完后分别查 PROC-B / PROC-C 两个池，
/// 断言批次只在 PROC-C 池里。
#[tokio::test]
async fn worker_scan_returned_advances_current_process_id() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2B").await;
    // 工序 B（起点）→ 工序 C（RETURNED 传的目标）
    let proc_b = seed_process(&pool, "PROC-B2", "工序B2").await;
    let proc_c = seed_process(&pool, "PROC-C2", "工序C2").await;
    let wt = insert_work_type(&pool, "WT-B2", "工种B2", Some(5)).await;
    // 工种**只**映射 proc_b（起点工序）。
    //
    // 关键：RETURNED 成功后同事务会调 `refill_for_worker`，而 refill 按
    // 「工种可加工工序池」抢批。若工种也映射 proc_c，refill 会立刻把刚归还的
    // 批次再抢回工人（location=WORKER）→ 断言「批次在 proc_c 池」必然失败。
    // 工种不含 proc_c → refill 抢不动它，批次留在 proc_c 候选池里可被端点查到。
    // RETURNED 本身不校验工种资格（只校验 `t_shelf_process` 货架映射，见下）。
    link_work_type_to_process(&pool, wt, proc_b).await;
    let prod_shelf = insert_shelf(&pool, "PROD-B2", "PROD-B2", "PRODUCTION").await;
    // 同一货架同时映射 B / C —— RETURNED 的 t_shelf_process 校验要求 shelf 映射 next_process_id
    link_shelf_to_process(&pool, prod_shelf, proc_b).await;
    link_shelf_to_process(&pool, prod_shelf, proc_c).await;

    let worker = insert_worker(&pool, "BC002B", "工2B", Some(wt)).await;
    // 工人持有 1 件 IN_PROCESS+WORKER 批次，current_process_id = proc_b（起点工序）
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-002B", worker, proc_b, 1, true).await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2b", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "H-002B",
                "badge_code": "BC002B",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": proc_c.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    assert_eq!(env["code"], 0);

    // DB 层：权威列必须被推进到目标工序
    let process_after: Option<i64> =
        sqlx::query_scalar("SELECT current_process_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("query current_process_id after RETURNED");
    assert_eq!(
        process_after,
        Some(proc_c),
        "RETURNED 应把 current_process_id 推进到 next_process_id（{proc_c}），\
         实际 {process_after:?} —— 不推进会让批次落回原工序池"
    );

    // 端点层：批次只应出现在 PROC-C 池，不应再出现在 PROC-B 池
    // （login_manager_with_username 会 INSERT t_user，只能调一次，后续复用 token）
    let (app, mgr) = login_manager_with_username(&pool, "admin_pool2b").await;
    let (sb, eb) = send(
        app.clone(),
        json_request("GET", &format!("/prod/pool/{proc_b}"), None, Some(&mgr)),
    )
    .await;
    assert_eq!(sb, StatusCode::OK, "GET pool/{proc_b}: {eb}");
    let in_b = eb["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .any(|it| it["batch_id"] == json!(held_batch.to_string()));
    assert!(
        !in_b,
        "RETURNED 推进到 {proc_c} 后，批次不应再出现在原工序 {proc_b} 池: {eb}"
    );

    let (sc, ec) = send(
        app,
        json_request("GET", &format!("/prod/pool/{proc_c}"), None, Some(&mgr)),
    )
    .await;
    assert_eq!(sc, StatusCode::OK, "GET pool/{proc_c}: {ec}");
    let in_c = ec["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .any(|it| it["batch_id"] == json!(held_batch.to_string()));
    assert!(
        in_c,
        "RETURNED 推进后批次应出现在目标工序 {proc_c} 池: {ec}"
    );
}

/// 场景 2d（2026-10-04 回归）: worker-scan RETURNED 在 part **无工艺链**时也必须成功。
///
/// ## 这条测试补的是哪个洞
/// `t_part.process_chain_id` 是可空列（baseline 列 COMMENT：「NULL = 未制定工艺链」），
/// `worker_scan.rs` 必须按 `Option` 收它 —— 按 `i64` 解码会让工人归还手写工单（无链，
/// 工单域的常态）时必撞
/// `error occurred while decoding column 0: unexpected null; try decoding as an Option`
/// 整笔 500。本测试走 fixture 的 `with_chain=false` 分支（`t_part.process_chain_id`
/// 保持 NULL）覆盖这条主路径。
///
/// ## 断言
/// 1. 前置：`t_part.process_chain_id IS NULL`（防 fixture 未来被改成有链而假绿）
/// 2. `POST /prod/batches/worker-scan`（RETURNED）→ HTTP 200 + `code=0`
/// 3. `current_process_id` 推进到 `next_process_id`（RETURNED 的主状态变更）
/// 4. `current_process_step_id` 保留原值（走 else 分支）
///
/// ## 断言 4 的诚实边界
/// `mark_batch_returned` 的 `current_process_step_id` 形参带 `_` 前缀、SQL 里
/// **不写**该列（2026-09-30 起的已知缺口，只影响显示），所以「保留原值」
/// 无论 else 分支返回什么都成立 —— 它是**防回归的护栏**（挡住将来有人改成写 NULL），
/// 不是 else 分支确实执行过的证明。真正的回归信号是断言 2 的 HTTP 200。
#[tokio::test]
async fn worker_scan_returned_without_process_chain_succeeds() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2D").await;
    // 起点工序（工种可加工）→ RETURNED 传的目标工序（工种**不含**，否则同事务的
    // refill 会把刚归还的批次又抢回工人，干扰断言）
    let proc_b = seed_process(&pool, "PROC-B3", "工序B3").await;
    let proc_c = seed_process(&pool, "PROC-C3", "工序C3").await;
    let wt = insert_work_type(&pool, "WT-B3", "工种B3", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc_b).await;
    let prod_shelf = insert_shelf(&pool, "PROD-B3", "PROD-B3", "PRODUCTION").await;
    // RETURNED 的 20507 校验要求目标货架映射 next_process_id（= proc_c）
    link_shelf_to_process(&pool, prod_shelf, proc_c).await;

    let worker = insert_worker(&pool, "BC002D", "工2D", Some(wt)).await;
    // with_chain = false ⇒ t_part.process_chain_id 为 NULL；批次仍带一个**非 NULL**
    // 的旧 current_process_step_id，好让断言 4 有东西可保留
    let (_held_part, held_batch, old_step) =
        insert_worker_held_part(&pool, customer, "H-002D", worker, proc_b, 1, false).await;

    // 前置守卫：fixture 的 `with_chain=false` 失效的话，本测试会变成假绿，先在这里 fail
    let chain_id: Option<i64> = sqlx::query_scalar(
        "SELECT p.process_chain_id FROM t_part p \
         JOIN t_part_batch b ON b.part_id = p.id WHERE b.id = $1",
    )
    .bind(held_batch)
    .fetch_one(&pool)
    .await
    .expect("query part process_chain_id");
    assert!(
        chain_id.is_none(),
        "fixture 前置不成立：part 绑了工艺链 {chain_id:?}，本测试就测不到无链分支了"
    );

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2d", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "H-002D",
                "badge_code": "BC002D",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": proc_c.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    // 这是本测试的核心断言：按 `i64` 解码 `process_chain_id` 时这里会 500
    // （`decoding column 0: unexpected null`，part 无工艺链）
    assert_eq!(s, StatusCode::OK, "scan RETURNED（part 无工艺链）: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_RETURNED");

    let after: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(held_batch)
    .fetch_one(&pool)
    .await
    .expect("query batch after RETURNED");
    assert_eq!(
        after.0,
        Some(proc_c),
        "RETURNED 应把 current_process_id 推进到 next_process_id({proc_c})，实际 {:?}",
        after.0
    );
    assert_eq!(
        after.1,
        Some(old_step),
        "part 无工艺链时 RETURNED 应保留批次既有 current_process_step_id({old_step})，实际 {:?}",
        after.1
    );
}

/// 场景 3: 池空时 refill 返回 empty + pool_empty=true
#[tokio::test]
async fn refill_when_pool_empty_returns_empty() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL3").await;
    let proc = seed_process(&pool, "PROC-C", "工序C").await;
    let wt = insert_work_type(&pool, "WT-C", "工种C", Some(10)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-C", "PROD-C", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC003", "工3", Some(wt)).await;
    // worker 当前不持有任何，池里只有 1 件（max=10，refill 拿走 1 后池空）
    let (_pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-003", prod_shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin3").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin refill: {env}");
    assert_eq!(env["code"], 0);
    // 池里只 1 件，refill 取走 1 → taken.len()=1, pool_empty=false
    let taken = env["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken.len(), 1, "应 taken=1: {env}");
    assert_eq!(env["data"]["pool_empty"], false);

    // 第二次 refill 池空 → taken=0 + pool_empty=true
    let (app, token) = login_manager_with_username(&pool, "admin3b").await;
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "second refill: {env2}");
    let taken2 = env2["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken2.len(), 0, "应 taken=0: {env2}");
    assert_eq!(env2["data"]["pool_empty"], true);
}

/// 场景 4: refill 上限 = max_held_batches
#[tokio::test]
async fn refill_caps_at_max_held_batches() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL4").await;
    let proc = seed_process(&pool, "PROC-D", "工序D").await;
    let wt = insert_work_type(&pool, "WT-D", "工种D", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-D", "PROD-D", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC004", "工4", Some(wt)).await;
    // 池里塞 20 件，max=5 → refill 应只抢 5
    for i in 0..20 {
        let sn = format!("P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, prod_shelf, proc, 1).await;
    }

    let (app, token) = login_manager_with_username(&pool, "admin4").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin refill: {env}");
    let taken = env["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken.len(), 5, "max=5，应 taken=5: {env}");
    let held = count_held_by_worker(&pool, worker).await;
    assert_eq!(held, 5, "worker 持有 = 5");
}

/// 场景 5: 并发 refill 不双抢（`#[ignore]`：缺并发 app 基建）
#[tokio::test]
#[ignore = "需要 app-level 并发基建（多 axum server 共享同一 pool）；当前测试用单 Router oneshot，无法验证 SKIP LOCKED 跨事务隔离"]
async fn concurrent_refill_no_double_pick() {
    // 见 README：take_one_from_pool 的 CTE 内含 FOR UPDATE SKIP LOCKED，
    // 单 SQL 原子守卫「held < max」+「row-level skip」，因此两个并发事务
    // 抢同一池时各自拿不同批。本测试需在多 tokio task 中同时调
    // /admin/worker-pool/refill，并断言「两次 taken 总数 = 池大小 + 两次 taken 无交集」。
    // 当前基础设施（单 Router + oneshot）不支持并发，需引入多 Router 共享 state。
}

/// 场景 6: refill 限定 shelf 范围（worker 只能从所绑 shelf 的池里抢）
#[tokio::test]
async fn refill_respects_shelf_scope() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL6").await;
    let proc = seed_process(&pool, "PROC-F", "工序F").await;
    let wt = insert_work_type(&pool, "WT-F", "工种F", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf_a = insert_shelf(&pool, "PROD-F1", "PROD-F1", "PRODUCTION").await;
    let shelf_b = insert_shelf(&pool, "PROD-F2", "PROD-F2", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_a, proc).await;
    link_shelf_to_process(&pool, shelf_b, proc).await;

    let worker = insert_worker(&pool, "BC006", "工6", Some(wt)).await;
    // shelf_b 上有 2 件，shelf_a 上有 0 件
    for i in 0..2 {
        let sn = format!("PB-{:03}", i);
        insert_pool_part(&pool, customer, &sn, shelf_b, proc, 1).await;
    }
    // refill 时指定 shelf=shelf_a → 池空（shelf_a 上没件）
    let (app, token) = login_manager_with_username(&pool, "admin6").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": shelf_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "refill scope: {env}");
    let taken = env["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken.len(), 0, "shelf_a 上没件，应 taken=0: {env}");
    assert_eq!(env["data"]["pool_empty"], true);
}

/// 场景 7: refill 跳过并发修改的批次（`#[ignore]`：需并发模拟基建）
#[tokio::test]
#[ignore = "需在另一个事务里 UPDATE t_part_batch.version / status 后再触发 refill 并断言 take_one_from_pool 返回 None"]
async fn refill_skips_concurrently_modified_batch() {
    // 设计意图：另一个事务先把候选 batch 的 version+1，refill 的 CTE
    // 用 `pb.version = candidate.version` 守卫，0 行 → 视为池空。
    // 当前 test harness 单 transaction 串行，无法模拟该窗口。
}

/// 场景 8: worker-scan 越权 shelf → 40301 SHELF_MISMATCH
#[tokio::test]
async fn worker_scan_shelf_scope_violation_403() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL8").await;
    let proc = seed_process(&pool, "PROC-H", "工序H").await;
    let wt = insert_work_type(&pool, "WT-H", "工种H", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf_x = insert_shelf(&pool, "PROD-H1", "PROD-H1", "PRODUCTION").await;
    let shelf_y = insert_shelf(&pool, "PROD-H2", "PROD-H2", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_x, proc).await;
    link_shelf_to_process(&pool, shelf_y, proc).await;

    let worker = insert_worker(&pool, "BC008", "工8", Some(wt)).await;
    let (_held_part, _held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-008", worker, proc, 1, true).await;

    // user 只绑定 shelf_x，请求扫描到 shelf_y → 40301 SHELF_MISMATCH
    let (app, token, _pool) = login_shelf_account(pool.clone(), "user8", &[shelf_x]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "H-008",
                "badge_code": "BC008",
                "event_type": "RETURNED",
                "shelf_id": shelf_y.to_string(),
                "next_process_id": proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "shelf 越权: {env}");
    assert_eq!(env["code"], 40301, "SHELF_MISMATCH: {env}");
}

/// 场景 9: take 更新 t_part.current_holder_id = worker_id
#[tokio::test]
async fn take_updates_t_part_holder() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL9").await;
    let proc = seed_process(&pool, "PROC-I", "工序I").await;
    let wt = insert_work_type(&pool, "WT-I", "工种I", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-I", "PROD-I", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC009", "工9", Some(wt)).await;
    let (_pool_part, pool_batch) =
        insert_pool_part(&pool, customer, "P-009", prod_shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin9").await;
    let (_s, _env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;

    // 2026-09-16 PR-2（migration 027）：t_part 删 `current_holder_id` / `location`；
    // 「take 更新 holder」改由 t_part_batch 承担，断言目标同步改写为 t_part_batch。
    let holder: Option<i64> = sqlx::query_scalar!(
        "SELECT current_holder_id FROM t_part_batch WHERE id = $1",
        pool_batch,
    )
    .fetch_one(&pool)
    .await
    .expect("query holder");
    assert_eq!(
        holder,
        Some(worker),
        "t_part_batch holder 应被更新为 worker.id"
    );
    let loc: String = sqlx::query_scalar!(
        "SELECT location AS \"loc!\" FROM t_part_batch WHERE id = $1",
        pool_batch,
    )
    .fetch_one(&pool)
    .await
    .expect("query location");
    assert_eq!(loc, "WORKER", "t_part_batch.location 应=WORKER");
}

/// 场景 10: take 不更新 placed_at
#[tokio::test]
async fn take_does_not_update_placed_at() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL10").await;
    let proc = seed_process(&pool, "PROC-J", "工序J").await;
    let wt = insert_work_type(&pool, "WT-J", "工种J", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-J", "PROD-J", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC010", "工10", Some(wt)).await;
    let (_pool_part, pool_batch) =
        insert_pool_part(&pool, customer, "P-010", prod_shelf, proc, 1).await;

    // 2026-09-16 PR-3 批次 step 化：t_part_batch.placed_at 列已删；
    // 不再断言 take 前后时间。本测试名（take_does_not_update_placed_at）
    // 同步改为 take_does_not_change_state，与 PR-3 语义对齐。
    // 取 take 前 batch version（用作对比 baseline）
    let before_version: i32 =
        sqlx::query_scalar!("SELECT version FROM t_part_batch WHERE id = $1", pool_batch,)
            .fetch_one(&pool)
            .await
            .expect("query version");
    let _ = before_version;

    let (app, token) = login_manager_with_username(&pool, "admin10").await;
    let (_s, _env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;

    // PR-3 批次 step 化：t_part_batch.placed_at 列已删；改测 take 后 batch
    // 状态保持原状（IN_PROCESS + current_process_step_id 不变）。
    // 2026-09-30：同样断言 current_process_id 不变（池内移动工序不变）。
    let (status_after, step_after, process_after): (String, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT status, current_process_step_id, current_process_id \
             FROM t_part_batch WHERE id = $1",
        )
        .bind(pool_batch)
        .fetch_one(&pool)
        .await
        .expect("query after");
    assert_eq!(
        status_after, "IN_PROCESS",
        "take 后 batch status 仍为 IN_PROCESS（fixture 起点）"
    );
    assert!(
        step_after.is_some(),
        "take 后 batch 仍持有 step（fixture 起点有 step_id）"
    );
    assert_eq!(
        process_after,
        Some(proc),
        "take 不应改变 current_process_id（池内移动工序不变）"
    );
}

/// 场景 11: t_part_event 持久化 TAKEN_FROM_POOL / RETURNED_TO_SHELF / SENT_TO_INSPECTION
#[tokio::test]
async fn events_persisted_to_t_part_event() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL11").await;
    let proc = seed_process(&pool, "PROC-K", "工序K").await;
    let wt = insert_work_type(&pool, "WT-K", "工种K", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-K", "PROD-K", "PRODUCTION").await;
    let insp_shelf = insert_shelf(&pool, "INSP-K", "INSP-K", "INSPECTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC011", "工11", Some(wt)).await;
    let (held_part, _held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-011", worker, proc, 1, true).await;
    let (pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-011", prod_shelf, proc, 1).await;

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "user11", &[prod_shelf, insp_shelf]).await;
    let (_s, _env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "H-011",
                "badge_code": "BC011",
                "event_type": "INSPECTED",
                "shelf_id": prod_shelf.to_string(),
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;

    // 验证两个 part 各有预期事件
    // held_part：SENT_TO_INSPECTION（worker-scan INSPECTED 时写）
    let held_events: Vec<String> = sqlx::query_scalar!(
        r#"SELECT event_type AS "event_type!"
        FROM t_part_event WHERE part_id = $1 ORDER BY created_at ASC, id ASC"#,
        held_part,
    )
    .fetch_all(&pool)
    .await
    .expect("query held events");
    assert!(
        held_events.iter().any(|e| e == "SENT_TO_INSPECTION"),
        "held_part 应有 SENT_TO_INSPECTION 事件: {held_events:?}"
    );
    // pool_part：TAKEN_FROM_POOL（refill 后写）
    let pool_events: Vec<String> = sqlx::query_scalar!(
        r#"SELECT event_type AS "event_type!"
        FROM t_part_event WHERE part_id = $1 ORDER BY created_at ASC, id ASC"#,
        pool_part,
    )
    .fetch_all(&pool)
    .await
    .expect("query pool events");
    assert!(
        pool_events.iter().any(|e| e == "TAKEN_FROM_POOL"),
        "pool_part 应有 TAKEN_FROM_POOL 事件: {pool_events:?}"
    );
}

/// 场景 12: admin refill 端点
#[tokio::test]
async fn admin_refill_endpoint_works() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL12").await;
    let proc = seed_process(&pool, "PROC-L", "工序L").await;
    let wt = insert_work_type(&pool, "WT-L", "工种L", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-L", "PROD-L", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC012", "工12", Some(wt)).await;
    for i in 0..5 {
        let sn = format!("P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, prod_shelf, proc, 1).await;
    }

    let (app, token) = login_manager_with_username(&pool, "admin12").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin refill: {env}");
    assert_eq!(env["code"], 0);
    let taken = env["data"]["taken"].as_array().expect("data.taken");
    assert_eq!(taken.len(), 3, "max=3，应 taken=3: {env}");
    let held = count_held_by_worker(&pool, worker).await;
    assert_eq!(held, 3);
}

/// 场景 13 (2026-09-30 重构): move WORKER → POOL 把持有批次放回候选池。
///
/// 2026-09-30 之前：原 `admin_remove` 端点。重构后走统一 `POST /prod/pool/move`
/// 端点，`from.kind=WORKER, to.kind=POOL` 方向。验证：
/// - batch 回到 PRODUCTION_SHELF + holder=shelf
/// - **current_process_step_id 不变**（move 不推进工序链）
/// - 响应 `MoveResult { from_kind=WORKER, to_kind=POOL, new_location="PRODUCTION_SHELF" }`
#[tokio::test]
async fn move_worker_to_pool_returns_batch_to_pool() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL13").await;
    let proc = seed_process(&pool, "PROC-M", "工序M").await;
    let wt = insert_work_type(&pool, "WT-M", "工种M", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-M", "PROD-M", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC013", "工13", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-013", worker, proc, 1, true).await;

    // 取 move 前的 step_id / current_process_id（move 后都应保持不变）
    let (step_before, process_before): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_step_id, current_process_id FROM t_part_batch WHERE id = $1",
    )
    .bind(held_batch)
    .fetch_one(&pool)
    .await
    .expect("query step/process before");

    let (app, token) = login_manager_with_username(&pool, "admin13").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                "from": { "kind": "WORKER", "worker_id": worker.to_string() },
                "to":   { "kind": "POOL",   "shelf_id": prod_shelf.to_string() },
                "note": "退换料"
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "move WORKER→POOL: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["from_kind"], "WORKER");
    assert_eq!(env["data"]["to_kind"], "POOL");
    assert_eq!(env["data"]["new_location"], "PRODUCTION_SHELF");
    assert_eq!(env["data"]["new_holder_id"], prod_shelf.to_string());
    assert_eq!(env["data"]["batch_id"], held_batch.to_string());

    // worker 应不再持有该批次（count=0）
    let held = count_held_by_worker(&pool, worker).await;
    assert_eq!(held, 0, "move WORKER→POOL 后 worker 应释放该批次");
    // batch 应回到 PRODUCTION_SHELF holder=shelf
    let row = sqlx::query!(
        r#"SELECT location AS "loc!", current_holder_id AS "ch?",
                  current_process_step_id AS "step?", current_process_id AS "pid?"
        FROM t_part_batch WHERE id = $1"#,
        held_batch,
    )
    .fetch_one(&pool)
    .await
    .expect("query batch");
    assert_eq!(row.loc, "PRODUCTION_SHELF");
    assert_eq!(row.ch, Some(prod_shelf));
    // 关键不变量：move 不推进工序链
    assert_eq!(
        row.step, step_before,
        "move 不应改变 current_process_step_id（step_before={step_before:?} step_after={:?}",
        row.step
    );
    // 2026-09-30 镜像断言：move 是池内移动 → 工序不变
    assert_eq!(
        row.pid, process_before,
        "move 不应改变 current_process_id（process_before={process_before:?} process_after={:?}",
        row.pid
    );
}

/// 场景 13b (2026-09-30 新增): move POOL → WORKER 把候选批次分配给 worker。
///
/// 验证：
/// - batch 移到 worker (location=WORKER + current_holder_id=worker_id)
/// - response 含 `current_held` / `max_held` 字段
/// - TAKEN_FROM_POOL / MOVED 事件写入
#[tokio::test]
async fn move_pool_to_worker_assigns_batch() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL13B").await;
    let proc = seed_process(&pool, "PROC-MB", "工序MB").await;
    let wt = insert_work_type(&pool, "WT-MB", "工种MB", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-MB", "PROD-MB", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC013B", "工13B", Some(wt)).await;
    let (_pool_part, pool_batch) =
        insert_pool_part(&pool, customer, "P-013B", prod_shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin13B").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/move",
            Some(json!({
                "batch_id": pool_batch.to_string(),
                "from": { "kind": "POOL",   "shelf_id": prod_shelf.to_string() },
                "to":   { "kind": "WORKER", "worker_id": worker.to_string() },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "move POOL→WORKER: {env}");
    assert_eq!(env["data"]["from_kind"], "POOL");
    assert_eq!(env["data"]["to_kind"], "WORKER");
    assert_eq!(env["data"]["new_location"], "WORKER");
    assert_eq!(env["data"]["new_holder_id"], worker.to_string());
    assert_eq!(env["data"]["current_held"], 1);
    assert_eq!(env["data"]["max_held"], 3);

    // DB 验证
    let row = sqlx::query!(
        r#"SELECT location AS "loc!", current_holder_id AS "ch?" FROM t_part_batch WHERE id = $1"#,
        pool_batch,
    )
    .fetch_one(&pool)
    .await
    .expect("query");
    assert_eq!(row.loc, "WORKER");
    assert_eq!(row.ch, Some(worker));
    let held = count_held_by_worker(&pool, worker).await;
    assert_eq!(held, 1);
}

/// 场景 13c (2026-09-30 新增): move WORKER → WORKER 把批次从一个工人切到另一个。
#[tokio::test]
async fn move_worker_to_worker_transfers_batch() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL13C").await;
    let proc = seed_process(&pool, "PROC-MC", "工序MC").await;
    let wt = insert_work_type(&pool, "WT-MC", "工种MC", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-MC", "PROD-MC", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker_src = insert_worker(&pool, "BC013C1", "工13C1", Some(wt)).await;
    let worker_dst = insert_worker(&pool, "BC013C2", "工13C2", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-013C", worker_src, proc, 1, true).await;

    let (app, token) = login_manager_with_username(&pool, "admin13C").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                "from": { "kind": "WORKER", "worker_id": worker_src.to_string() },
                "to":   { "kind": "WORKER", "worker_id": worker_dst.to_string() },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "move WORKER→WORKER: {env}");
    assert_eq!(env["data"]["from_kind"], "WORKER");
    assert_eq!(env["data"]["to_kind"], "WORKER");
    assert_eq!(env["data"]["new_holder_id"], worker_dst.to_string());

    // 源 worker 不再持有
    assert_eq!(count_held_by_worker(&pool, worker_src).await, 0);
    // 目标 worker 持有 1 批
    assert_eq!(count_held_by_worker(&pool, worker_dst).await, 1);
}

/// 场景 13d (2026-09-30 新增): move from 与 batch 实际状态不一致 → 40904。
#[tokio::test]
async fn move_from_mismatch_returns_location_mismatch_error() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL13D").await;
    let proc = seed_process(&pool, "PROC-MD", "工序MD").await;
    let wt = insert_work_type(&pool, "WT-MD", "工种MD", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-MD", "PROD-MD", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    // batch 在 pool（不是 worker 持有）
    let (_pool_part, pool_batch) =
        insert_pool_part(&pool, customer, "P-013D", prod_shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin13D").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/move",
            Some(json!({
                "batch_id": pool_batch.to_string(),
                // from 谎报成 WORKER（实际在 POOL），期望 40904
                "from": { "kind": "WORKER", "worker_id": "999999999" },
                "to":   { "kind": "POOL",   "shelf_id": prod_shelf.to_string() },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "from 不匹配应 409: {env}");
    assert_eq!(
        env["code"], 20122,
        "BIZ_BATCH_LOCATION_MISMATCH 应 20122: {env}"
    );
}

/// 场景 13e (2026-09-30 新增): move 目标 worker 容量超限 → 409。
#[tokio::test]
async fn move_target_worker_capacity_exceeded() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL13E").await;
    let proc = seed_process(&pool, "PROC-ME", "工序ME").await;
    let wt = insert_work_type(&pool, "WT-ME", "工种ME", Some(1)).await; // max=1
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-ME", "PROD-ME", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker_dst = insert_worker(&pool, "BC013E-DST", "工13E-DST", Some(wt)).await;
    // 目标 worker 已持 1 批（触顶）
    let (_held_part_dst, _held_batch_dst, _step) =
        insert_worker_held_part(&pool, customer, "H-DST", worker_dst, proc, 1, true).await;

    let worker_src = insert_worker(&pool, "BC013E-SRC", "工13E-SRC", Some(wt)).await;
    let (_held_part_src, held_batch_src, _step) =
        insert_worker_held_part(&pool, customer, "H-SRC", worker_src, proc, 1, true).await;

    let (app, token) = login_manager_with_username(&pool, "admin13E").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/move",
            Some(json!({
                "batch_id": held_batch_src.to_string(),
                "from": { "kind": "WORKER", "worker_id": worker_src.to_string() },
                "to":   { "kind": "WORKER", "worker_id": worker_dst.to_string() },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "容量超限应 409: {env}");
    assert_eq!(
        env["code"], 20204,
        "BIZ_WORKER_HOLD_LIMIT_EXCEEDED 应 20204: {env}"
    );

    // 源 worker 应仍持有（事务回滚）
    assert_eq!(count_held_by_worker(&pool, worker_src).await, 1);
}

/// 场景 13f (2026-09-30 新增): move 同 kind 移动（POOL→POOL）→ 422 VALIDATION_ERROR。
#[tokio::test]
async fn move_same_kind_rejected_with_validation_error() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL13F").await;
    let proc = seed_process(&pool, "PROC-MF", "工序MF").await;
    let wt = insert_work_type(&pool, "WT-MF", "工种MF", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-MF", "PROD-MF", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let (_pp, batch) = insert_pool_part(&pool, customer, "P-013F", prod_shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin13F").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/move",
            Some(json!({
                "batch_id": batch.to_string(),
                "from": { "kind": "POOL", "shelf_id": prod_shelf.to_string() },
                "to":   { "kind": "POOL", "shelf_id": prod_shelf.to_string() },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "同 kind 移动应 422: {env}"
    );
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
}

/// 场景 14: refill 失败回滚 worker-scan（`#[ignore]`：DB 故障注入缺基建）
#[tokio::test]
#[ignore = "需要 DB 故障注入（人为断网 / 临时约束 / DROP TABLE mid-tx）来制造 refill 失败而 scan 成功的窗口；当前 test harness 无法注入"]
async fn refill_failure_rolls_back_worker_scan() {
    // 设计意图：refill_for_worker 与 worker_scan_event 共享 handler 内
    // begin() 的同一事务，refill 抛错 → 事务自动回滚 → scan 的
    // RETURNED_TO_SHELF / SENT_TO_INSPECTION 事件日志 + 状态翻转都应一并撤销。
    // 测试需要一种可控方式让 refill 内部失败（其它分支成功），例如：
    //   1. 先把 work_type 的 process 映射删掉（清空 t_work_type_process）
    //      → refill 进入「BIZ_WORK_TYPE_NO_PROCESS_MAPPING」分支抛错；
    //      但 scan 部分已写入事件日志；
    //   2. 验证：events 表里应无该 part 的新事件日志；
    //      part.location / current_holder_id 应保持原状。
    //   实现这一窗口需要在 scan 与 refill 中间时点突变 work_type 映射，
    //   而当前 refill 流程在 worker_scan_event *之后*（handler 层）调用，
    //   中间插入 mutation 需要重构或并发 tx。
}

/// 场景 15: work_type.max_held_batches = NULL → 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET
#[tokio::test]
async fn max_held_null_returns_error() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL15").await;
    let proc = seed_process(&pool, "PROC-O", "工序O").await;
    let wt = insert_work_type(&pool, "WT-O", "工种O", None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-O", "PROD-O", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC015", "工15", Some(wt)).await;
    insert_pool_part(&pool, customer, "P-015", prod_shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin15").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "max_held NULL 应 400 (BIZ 业务错默认 400): {env}"
    );
    assert_eq!(env["code"], 20904, "BIZ_WORK_TYPE_MAX_HELD_NOT_SET: {env}");
}

/// 场景 16: worker.work_type_id = NULL → 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET (走不到)
/// 实际：worker.work_type_id = NULL → BIZ_WORKER_NO_WORK_TYPE (20206)
#[tokio::test]
async fn worker_no_work_type_returns_error() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL16").await;
    let proc = seed_process(&pool, "PROC-P", "工序P").await;
    let wt = insert_work_type(&pool, "WT-P", "工种P", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-P", "PROD-P", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC016", "工16", None).await;
    insert_pool_part(&pool, customer, "P-016", prod_shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin16").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "无 work_type 应 400: {env}");
    assert_eq!(env["code"], 20206, "BIZ_WORKER_NO_WORK_TYPE: {env}");
}

// ===========================================================================
//  GET /api/v2/worker-pool/{process_id} —— by-process 候选池详情（Task 5）
//
//  生产路径 `/api/v2/worker-pool/{id}`（main.rs nest("/api/v2", v2_router())）；
//  集成测试 harness `tests/common::test_app` 直接用 `v2_router()` 不带
//  `/api/v2` nest，故测试 URI 是 `/worker-pool/{id}`。
// ===========================================================================

/// 插一个 L2 叶子客户（parent_id=L1.id）用于 customer_path 拼接测试。
///
/// 用 `sqlx::query`（runtime）而非 `query!`：避免每加一个 fixture 就跑
/// `cargo sqlx prepare` 重写 `.sqlx` 元数据（会清掉同 worktree 内其它测试文件
/// 仍在用的 cache，对其它 worktree 也有干扰）。
async fn insert_l2_customer(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, NULL, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(l1_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2 t_customer");
    id
}

/// 场景 H1: happy path —— 返回 process 元数据 + workers + work_types(含 max_held) +
/// 跨货架候选批次列表。排序：system_delivery_date ASC NULLS LAST → is_urgent DESC → id ASC。
#[tokio::test]
async fn pool_by_process_happy() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    // L1 + L2 客户（L2.parent_id = L1.id → 触发 "L1 / L2" 路径）
    let l1 = insert_customer_l2(&pool, "L1-NAME").await;
    let l2 = insert_l2_customer(&pool, "L2-NAME", l1).await;

    let proc = seed_process(&pool, "PROC-PBP", "工序PBP").await;
    let wt_a = insert_work_type(&pool, "WT-PBP-A", "工种A", Some(3)).await;
    let wt_b = insert_work_type(&pool, "WT-PBP-B", "工种B", None).await;
    link_work_type_to_process(&pool, wt_a, proc).await;
    link_work_type_to_process(&pool, wt_b, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-PBP", "PROD-PBP", "PRODUCTION").await;

    let _w_a = insert_worker(&pool, "BC-PBP-A", "工A", Some(wt_a)).await;
    let _w_b = insert_worker(&pool, "BC-PBP-B", "工B", Some(wt_b)).await;

    // 2 批次：urgent 在前（同 system_delivery_date 时 is_urgent DESC 排序在前）。
    // `insert_pool_part` 把 is_urgent 硬编码为 false —— 加急单用 UPDATE 翻成 true。
    // 用 `sqlx::query`（runtime）而非 `query!` 避免动 `.sqlx` cache（会清掉
    // 同 worktree 其它测试文件仍在用的离线 metadata）。
    let (p_urgent, _b_urgent) = insert_pool_part(&pool, l2, "U-001", prod_shelf, proc, 2).await;
    let (_p_normal, _b_normal) = insert_pool_part(&pool, l2, "N-001", prod_shelf, proc, 5).await;
    sqlx::query("UPDATE t_part SET is_urgent = true WHERE id = $1")
        .bind(p_urgent)
        .execute(&pool)
        .await
        .expect("mark U-001 urgent");

    let (app, token) = login_manager_with_username(&pool, "admin_pbp").await;
    // 注意：`tests/common::test_app` 用 `v2_router()`（不带 `/api/v2` nest，
    // 与 main.rs `nest("/api/v2", v2_router())` 不一样），所以测试 URI
    // 是 `/worker-pool/{id}` 而不是 `/api/v2/worker-pool/{id}`。
    let uri = format!("/prod/pool/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "pool_by_process happy: {env}");
    assert_eq!(env["code"], 0, "code 应 0: {env}");

    let data = &env["data"];
    assert_eq!(data["process_id"], proc.to_string());
    assert_eq!(data["process_code"], "PROC-PBP");
    assert_eq!(data["process_name"], "工序PBP");

    let workers = data["workers"].as_array().expect("workers array");
    assert_eq!(workers.len(), 2, "workers 应 2 个: {env}");

    let work_types = data["work_types"].as_array().expect("work_types array");
    assert_eq!(work_types.len(), 2, "work_types 应 2 个: {env}");
    // 找到 max_held_batches = Some(3) 与 None 的两条
    let has_three = work_types.iter().any(|w| w["max_held_batches"] == 3);
    let has_null = work_types.iter().any(|w| w["max_held_batches"].is_null());
    assert!(has_three && has_null, "max_held 应含 Some(3) + None: {env}");

    assert_eq!(data["total"], 2, "total 应 2: {env}");
    let items = data["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "items 应 2 个: {env}");
    // items[0] 应为加急单
    assert_eq!(
        items[0]["is_urgent"], true,
        "items[0] 应 is_urgent=true: {env}"
    );
    assert_eq!(items[0]["serial_no"], "U-001");
    // items[1] 应为非加急单
    assert_eq!(
        items[1]["is_urgent"], false,
        "items[1] 应 is_urgent=false: {env}"
    );
    assert_eq!(items[1]["serial_no"], "N-001");
    // customer_path 应为 "L1-NAME / L2-NAME"（2 级：fixture L1 是祖父，不入 path）
    assert_eq!(
        items[0]["customer_path"], "L1-NAME / L2-NAME",
        "customer_path 应拼成 L1-NAME / L2-NAME: {env}"
    );
    // shelf 元数据存在
    assert_eq!(items[0]["shelf_id"], prod_shelf.to_string());
    assert_eq!(items[0]["shelf_code"], "PROD-PBP");
    assert_eq!(items[0]["shelf_name"], "PROD-PBP");
}

/// 场景 H2: 不存在的 process_id → 20801 BIZ_PROCESS_NOT_FOUND + 404
#[tokio::test]
async fn pool_by_process_process_not_found() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-NF", "工序NF").await;
    let _wt = insert_work_type(&pool, "WT-NF", "工种NF", Some(3)).await;
    link_work_type_to_process(&pool, _wt, proc).await;
    // 一个不存在的 snowflake-style id（远大于实际生成）
    let nonexistent_id: i64 = 9_999_999_999_999;

    let (app, token) = login_manager_with_username(&pool, "admin_nf").await;
    let uri = format!("/prod/pool/{nonexistent_id}");
    let (s, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "不存在 process 应 404: {env}");
    assert_eq!(env["code"], 20801, "BIZ_PROCESS_NOT_FOUND: {env}");
}

/// 场景 H3: ShelfAccount 角色 → 40300 FORBIDDEN（service 守卫：Manager/Clerk/Inspector only）
#[tokio::test]
async fn pool_by_process_forbidden_for_shelf_account() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-FB", "工序FB").await;
    let wt = insert_work_type(&pool, "WT-FB", "工种FB", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-FB", "PROD-FB", "PRODUCTION").await;

    // ShelfAccount 绑一个 shelf（scope 必须给才能登录；调用端点时仍会被 service 拒绝）
    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "shelf_user_fb", &[prod_shelf]).await;
    let uri = format!("/prod/pool/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "ShelfAccount 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// 场景 H4: process 存在但无候选批次 → total=0, items=[]，元数据正常返回
#[tokio::test]
async fn pool_by_process_no_candidates_when_no_batch() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-EMPTY", "空工序").await;
    let wt = insert_work_type(&pool, "WT-EMPTY", "空工种", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let _w = insert_worker(&pool, "BC-EMPTY", "空工人", Some(wt)).await;

    let (app, token) = login_manager_with_username(&pool, "admin_empty").await;
    let uri = format!("/prod/pool/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "无 batch 应 200: {env}");
    assert_eq!(env["code"], 0, "code 应 0: {env}");

    let data = &env["data"];
    assert_eq!(data["process_id"], proc.to_string());
    assert_eq!(data["process_code"], "PROC-EMPTY");
    assert_eq!(data["process_name"], "空工序");
    assert_eq!(data["total"], 0, "total 应 0: {env}");
    let items = data["items"].as_array().expect("items array");
    assert_eq!(items.len(), 0, "items 应 []: {env}");
    // workers / work_types 字段应正常返回
    let workers = data["workers"].as_array().expect("workers array");
    assert_eq!(workers.len(), 1, "workers 应 1 个: {env}");
    let work_types = data["work_types"].as_array().expect("work_types array");
    assert_eq!(work_types.len(), 1, "work_types 应 1 个: {env}");
    assert_eq!(work_types[0]["max_held_batches"], 3);
}

// ===========================================================================
// 2026-09-30 重构：原 `admin/worker-pool/assign` 4 个端点已删除，被
// `POST /api/v2/prod/pool/move` 取代。覆盖：
//  - move_pool_to_worker_assigns_batch（场景 13b）：assign happy 路径
//  - move_target_worker_capacity_exceeded（场景 13e）：assign capacity 超限
//  - move_from_mismatch_returns_location_mismatch_error（场景 13d）：assign batch 不在池
//  - admin_assign_process_id_mismatch → 已废弃（move 端点显式校验 from/to 状态而非 process_id）
// ===========================================================================

// ===========================================================================
//  2026-09-29 CNC 重构 5 任务：has_cnc_program 字段 + 自动分配优先级测试
//
//  覆盖：
//   - take_one_from_pool_prefers_programmed_batch
//       同货架两个 batch（一个有 G_CODE 一个无），应优先 take 已编程 batch
//   - list_candidates_includes_has_cnc_program
//       PoolBatchItem 返回 has_cnc_program 字段
//   - held_batch_includes_has_cnc_program
//       HeldBatchItem 返回 has_cnc_program 字段（worker-pool state 端点）
// ===========================================================================

/// 为 part 插一个 t_part_file.kind='G_CODE' 行（worker_pool 候选池视图测试）。
async fn seed_g_code_for_part(pool: &PgPool, part_id: i64) -> i64 {
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
         'aabbccdd' || repeat('0', 56), $5, 0, $5, 0)",
    )
    .bind(id)
    .bind(part_id)
    .bind(object_key)
    .bind(format!("test_{id}.nc"))
    .bind(now)
    .execute(pool)
    .await
    .expect("seed g_code for part");
    id
}

/// take_one_from_pool 自动分配优先级：同货架两个 batch，一个有 G_CODE 一个无，
/// 应优先 take 已编程的（has_cnc_program DESC）。
///
/// 2026-09-29 新增：用 max_held_batches=1 限制 worker 持有数，refill 后取恰好 1 批；
/// 验证 taken[0] 是已上传 G_CODE 的 part（A-CNC）。
#[tokio::test]
async fn take_one_from_pool_prefers_programmed_batch() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL-CNC-PREF").await;
    let proc = seed_process(&pool, "PROC-CNC-PREF", "工序-CNC-优先级").await;
    // max_held_batches=1 限制 refill 只抢 1 批（避免后续断言失稳）
    let wt = insert_work_type(&pool, "WT-CNC-PREF", "工种-CNC", Some(1)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-CNC-PREF", "PROD-CNC-PREF", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC-CNC-PREF", "工CNC-优先级", Some(wt)).await;
    // 两个 part A/B 同交期 / 同加急 / 同货架（同 process_id），但 A 有 G_CODE，B 无
    let (_part_a, _batch_a) = insert_pool_part(&pool, customer, "A-CNC", prod_shelf, proc, 1).await;
    let (_part_b, _batch_b) = insert_pool_part(&pool, customer, "B-CNC", prod_shelf, proc, 1).await;
    // 给 A 插 G_CODE
    let part_a_id: i64 = sqlx::query_scalar("SELECT id FROM t_part WHERE serial_no = 'A-CNC'")
        .fetch_one(&pool)
        .await
        .expect("lookup part A");
    seed_g_code_for_part(&pool, part_a_id).await;
    // 触发 admin refill；返回 taken 应是 A（有 G_CODE）
    let (app, token) = login_manager_with_username(&pool, "admin_cnc_pref").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/pool/refill",
            Some(json!({
                "worker_id": worker.to_string(),
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "refill: {env}");
    let taken = env["data"]["taken"].as_array().expect("taken array");
    assert_eq!(taken.len(), 1, "应 taken=1（max_held=1）: {env}");
    // 验证：taken[0] 对应的 part serial_no 应是 'A-CNC'（已上传 G_CODE 优先）
    let taken_part_id: i64 = taken[0]["part_id"]
        .as_str()
        .expect("part_id is string")
        .parse()
        .expect("parse i64");
    let taken_serial: String = sqlx::query_scalar("SELECT serial_no FROM t_part WHERE id = $1")
        .bind(taken_part_id)
        .fetch_one(&pool)
        .await
        .expect("lookup serial");
    assert_eq!(
        taken_serial, "A-CNC",
        "应优先 take 已上传 G_CODE 的 part (A-CNC): {env}"
    );
    // has_cnc_program 字段透传
    assert_eq!(
        taken[0]["has_cnc_program"], true,
        "已上传 G_CODE 应透传 has_cnc_program=true: {env}"
    );
}

/// list_candidates_by_process_all_shelves 应在 items[*].has_cnc_program 透传实际值。
#[tokio::test]
async fn list_candidates_includes_has_cnc_program() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL-CNC-LIST").await;
    let proc = seed_process(&pool, "PROC-CNC-LIST", "工序-CNC-list").await;
    let wt = insert_work_type(&pool, "WT-CNC-LIST", "工种-CNC-list", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-CNC-LIST", "PROD-CNC-LIST", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;
    // part A 有 G_CODE，B 无
    let (_part_a, _batch_a) =
        insert_pool_part(&pool, customer, "A-LIST", prod_shelf, proc, 1).await;
    let (_part_b, _batch_b) =
        insert_pool_part(&pool, customer, "B-LIST", prod_shelf, proc, 1).await;
    let part_a_id: i64 = sqlx::query_scalar("SELECT id FROM t_part WHERE serial_no = 'A-LIST'")
        .fetch_one(&pool)
        .await
        .expect("lookup part A");
    seed_g_code_for_part(&pool, part_a_id).await;

    let (app, token) = login_manager_with_username(&pool, "admin_cnc_list").await;
    let uri = format!("/prod/pool/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None::<Value>, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "pool_by_process: {env}");
    let items = env["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "应 2 个候选: {env}");
    let mut found_a = false;
    let mut found_b = false;
    for it in items {
        let serial = sqlx::query_scalar::<_, String>("SELECT serial_no FROM t_part WHERE id = $1")
            .bind(it["part_id"].as_str().unwrap().parse::<i64>().unwrap())
            .fetch_one(&pool)
            .await
            .expect("lookup serial");
        match serial.as_str() {
            "A-LIST" => {
                found_a = true;
                assert_eq!(
                    it["has_cnc_program"], true,
                    "A 应有 has_cnc_program=true: {env}"
                );
            }
            "B-LIST" => {
                found_b = true;
                assert_eq!(
                    it["has_cnc_program"], false,
                    "B 应有 has_cnc_program=false: {env}"
                );
            }
            _ => {}
        }
    }
    assert!(found_a && found_b, "应同时找到 A 与 B: {env}");
}

/// `GET /prod/pool/state` 应在 `held_batches[*].has_cnc_program` 透传实际值。
///
/// 2026-09-29 补漏：前端 `WorkerQueueBoard.vue`「已编程」tag 渲染依赖
/// `HeldBatchItem.has_cnc_program` 字段。后端 model 与 SQL 必须真实返回 EXISTS(G_CODE) 值，
/// 否则 Zod strip 模式下前端静默丢字段会触发 schema 校验异常（`has_cnc_program` 必填）。
/// 场景：worker 持有 2 个 batch（A 有 G_CODE，B 无），断言 `held_batches[*].has_cnc_program`
/// 分别为 true / false。
#[tokio::test]
async fn held_batch_includes_has_cnc_program() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL-CNC-HELD").await;
    let proc = seed_process(&pool, "PROC-CNC-HELD", "工序-CNC-held").await;
    let wt = insert_work_type(&pool, "WT-CNC-HELD", "工种-CNC-held", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-CNC-HELD", "PROD-CNC-HELD", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC-CNC-HELD", "工CNC-held", Some(wt)).await;
    // 两个 held batch：A 有 G_CODE，B 无
    let (_part_a, _batch_a, _step_a) =
        insert_worker_held_part(&pool, customer, "H-CNC-A", worker, proc, 1, true).await;
    let (_part_b, _batch_b, _step_b) =
        insert_worker_held_part(&pool, customer, "H-CNC-B", worker, proc, 1, true).await;
    let part_a_id: i64 = sqlx::query_scalar("SELECT id FROM t_part WHERE serial_no = 'H-CNC-A'")
        .fetch_one(&pool)
        .await
        .expect("lookup part A");
    seed_g_code_for_part(&pool, part_a_id).await;

    // 调 state 端点：worker 当前持有 2 个 batch（无需 manager role，登录任意 user 即可）
    let (app, token) = login_manager_with_username(&pool, "admin_cnc_held").await;
    let uri = format!("/prod/pool/state?worker_id={worker}&shelf_id={prod_shelf}");
    let (s, env) = send(app, json_request("GET", &uri, None::<Value>, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "state: {env}");
    let held = env["data"]["held_batches"]
        .as_array()
        .expect("held_batches array");
    assert_eq!(held.len(), 2, "应 2 个 held batch: {env}");
    let mut found_a = false;
    let mut found_b = false;
    for it in held {
        let serial = sqlx::query_scalar::<_, String>("SELECT serial_no FROM t_part WHERE id = $1")
            .bind(it["part_id"].as_str().unwrap().parse::<i64>().unwrap())
            .fetch_one(&pool)
            .await
            .expect("lookup serial");
        match serial.as_str() {
            "H-CNC-A" => {
                found_a = true;
                assert_eq!(
                    it["has_cnc_program"], true,
                    "A 应有 has_cnc_program=true（已上传 G_CODE）: {env}"
                );
            }
            "H-CNC-B" => {
                found_b = true;
                assert_eq!(
                    it["has_cnc_program"], false,
                    "B 应有 has_cnc_program=false（未上传 G_CODE）: {env}"
                );
            }
            _ => {}
        }
    }
    assert!(found_a && found_b, "应同时找到 H-CNC-A 与 H-CNC-B: {env}");
}

// 2026-09-30 重构：原 `admin_assign_process_id_mismatch` 端点已删除，被 move 端点取代。
// 通过 from/to 显式校验状态而非 process_id；功能已合并到 move_pool_to_worker_assigns_batch（场景 13b）。

// ===========================================================================
//  GET /api/v2/prod/pool/counts —— 全工序候选批次聚合计数
//  （2026-09-30 新增，db78bba4 spec）
//
//  覆盖场景：
//   21. pool_counts_returns_aggregate_by_process   happy path: 3 个 process,
//       各塞不同数量的 IN_PROCESS+PRODUCTION_SHELF 批次，断言 counts[*].count
//       总和与 per-process 都对得上，total = sum(counts[].count)
// ===========================================================================

/// 场景 21: pool_counts 端点 happy path —— 返回全工序聚合。
///
/// - 3 个 process：A / B / C 各塞 1 / 2 / 3 批 IN_PROCESS+PRODUCTION_SHELF 批次
/// - 共 6 批；期望 counts[*].count = {A:1, B:2, C:3}，total = 6
/// - process_code / process_name 元数据透传
/// - counts 按 process_id ASC 稳定排序（repo GROUP BY ORDER BY 保证）
#[tokio::test]
async fn pool_counts_returns_aggregate_by_process() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL-COUNTS").await;

    // 3 个 process（互不映射，专注 process 维度聚合）
    let proc_a = seed_process(&pool, "PROC-CA", "工序A").await;
    let proc_b = seed_process(&pool, "PROC-CB", "工序B").await;
    let proc_c = seed_process(&pool, "PROC-CC", "工序C").await;

    let prod_a = insert_shelf(&pool, "PROD-CA", "PROD-CA", "PRODUCTION").await;
    let prod_b = insert_shelf(&pool, "PROD-CB", "PROD-CB", "PRODUCTION").await;
    let prod_c = insert_shelf(&pool, "PROD-CC", "PROD-CC", "PRODUCTION").await;

    // A: 1 件，B: 2 件，C: 3 件（每件用不同 serial_no 避免 pkey 冲突）
    insert_pool_part(&pool, customer, "PA-001", prod_a, proc_a, 1).await;
    insert_pool_part(&pool, customer, "PB-001", prod_b, proc_b, 1).await;
    insert_pool_part(&pool, customer, "PB-002", prod_b, proc_b, 1).await;
    insert_pool_part(&pool, customer, "PC-001", prod_c, proc_c, 1).await;
    insert_pool_part(&pool, customer, "PC-002", prod_c, proc_c, 1).await;
    insert_pool_part(&pool, customer, "PC-003", prod_c, proc_c, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_counts").await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/pool/counts", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "pool_counts happy: {env}");
    assert_eq!(env["code"], 0, "code 应 0: {env}");

    let counts = env["data"]["counts"].as_array().expect("counts array");
    assert_eq!(
        counts.len(),
        3,
        "应 3 个 process（含候选批次的 process）: {env}"
    );
    // counts 按 process_id ASC 排序（repo GROUP BY ORDER BY 保证）
    assert_eq!(
        counts[0]["process_id"],
        proc_a.to_string(),
        "counts[0] 应为 proc_a: {env}"
    );
    assert_eq!(counts[0]["process_code"], "PROC-CA");
    assert_eq!(counts[0]["process_name"], "工序A");
    assert_eq!(counts[0]["count"], 1, "A 应 1 件: {env}");

    assert_eq!(
        counts[1]["process_id"],
        proc_b.to_string(),
        "counts[1] 应为 proc_b: {env}"
    );
    assert_eq!(counts[1]["process_code"], "PROC-CB");
    assert_eq!(counts[1]["count"], 2, "B 应 2 件: {env}");

    assert_eq!(
        counts[2]["process_id"],
        proc_c.to_string(),
        "counts[2] 应为 proc_c: {env}"
    );
    assert_eq!(counts[2]["process_code"], "PROC-CC");
    assert_eq!(counts[2]["count"], 3, "C 应 3 件: {env}");

    // total = sum(counts[].count) = 1 + 2 + 3 = 6
    assert_eq!(env["data"]["total"], 6, "total 应 6: {env}");
}

/// pool_counts：含 0 候选批次的 process 不出现在 counts 中（GROUP BY 不输出 0 行）。
#[tokio::test]
async fn pool_counts_excludes_zero_count_processes() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL-COUNTS-EMPTY").await;

    // proc_empty：只有 process 定义，无任何 pool 批次
    let _proc_empty = seed_process(&pool, "PROC-EMPTY", "空工序").await;
    // proc_one：1 件候选批次
    let proc_one = seed_process(&pool, "PROC-ONE", "单件工序").await;
    let prod_one = insert_shelf(&pool, "PROD-ONE", "PROD-ONE", "PRODUCTION").await;
    insert_pool_part(&pool, customer, "P-ONE-001", prod_one, proc_one, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_counts_empty").await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/pool/counts", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "pool_counts empty: {env}");
    let counts = env["data"]["counts"].as_array().expect("counts array");
    assert_eq!(
        counts.len(),
        1,
        "应仅 1 个 process（含候选批次的 proc_one）: {env}"
    );
    assert_eq!(counts[0]["process_id"], proc_one.to_string());
    assert_eq!(counts[0]["count"], 1);
    assert_eq!(env["data"]["total"], 1);
}

/// pool_counts：跨多个货架聚合（同 process 在多个 shelf 上都有候选）。
#[tokio::test]
async fn pool_counts_aggregates_across_shelves() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL-COUNTS-MULTI").await;

    let proc = seed_process(&pool, "PROC-MULTI", "跨货架工序").await;
    // 2 个货架各自映射同一 process
    let shelf_x = insert_shelf(&pool, "PROD-MX", "PROD-MX", "PRODUCTION").await;
    let shelf_y = insert_shelf(&pool, "PROD-MY", "PROD-MY", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_x, proc).await;
    link_shelf_to_process(&pool, shelf_y, proc).await;

    // shelf_x: 2 件，shelf_y: 3 件 → 该 process 应聚合为 5 件
    insert_pool_part(&pool, customer, "MX-001", shelf_x, proc, 1).await;
    insert_pool_part(&pool, customer, "MX-002", shelf_x, proc, 1).await;
    insert_pool_part(&pool, customer, "MY-001", shelf_y, proc, 1).await;
    insert_pool_part(&pool, customer, "MY-002", shelf_y, proc, 1).await;
    insert_pool_part(&pool, customer, "MY-003", shelf_y, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_counts_multi").await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/pool/counts", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "pool_counts multi: {env}");
    let counts = env["data"]["counts"].as_array().expect("counts array");
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0]["process_id"], proc.to_string());
    assert_eq!(counts[0]["count"], 5, "应聚合跨货架 2 + 3 = 5 件: {env}");
    assert_eq!(env["data"]["total"], 5);
}

/// pool_counts：ShelfAccount 角色 → 40300 FORBIDDEN（service 守卫：Manager/Clerk/Inspector only）。
#[tokio::test]
async fn pool_counts_forbidden_for_shelf_account() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-FB-C", "工序FB-C").await;
    let _wt = insert_work_type(&pool, "WT-FB-C", "工种FB-C", Some(3)).await;
    link_work_type_to_process(&pool, _wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-FB-C", "PROD-FB-C", "PRODUCTION").await;

    // ShelfAccount 绑一个 shelf（scope 必须给才能登录；调用端点时仍会被 service 拒绝）
    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "shelf_user_counts", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/pool/counts", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "ShelfAccount 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}
