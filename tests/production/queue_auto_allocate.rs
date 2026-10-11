//! prod::queue::auto_allocate 端到端集成测试（part-worker-pool-federated-rocket 2026-09-11）
//!
//! ## 2026-10-15 改写：`mode` / `fill_ratio` 下线，target = 工种自动上限
//!
//! `POST /prod/queue/auto-allocate` 删除 `mode`（COUNT / TIME）与 `fill_ratio`，
//! 入参只剩 `{ process_id, shelf_id }`；每个 worker 的 target =
//! `t_work_type.auto_allocate_limit`（未配置兜底 `max_held_batches`）。原
//! TIME 模式 / fill_ratio 的 4 个场景（含 20703 / 20704 断言）随之下线。
//!
//! 覆盖场景：
//!   1. auto=2 / max=5 + 池 10 → `target=2`、`filled_count=2`；响应**不含**
//!      `mode` / `fill_ratio` 两个键（破坏性 wire 变更的显式断言）
//!   2. auto=NULL → target 兜底 max（max=5 + 池 10 → `target=5`、抢 5）
//!   3. 工人已持有 2 批、auto=2 / max=5、池里还有货 → `skipped_reason=AUTO_LIMIT_REACHED`
//!      （**不是** `WORKER_FULL` —— 硬上限还有名额，手动路径仍可取）
//!   4. max=NULL（auto 也 NULL）→ `skipped_reason=MAX_BATCHES_NOT_SET` 且 `target=0`
//!      （批量端点不因一个未配工种整体失败）
//!   5. 池空 → `pool_empty=true`
//!   6. process_id 不存在 → 20801 BIZ_PROCESS_NOT_FOUND
//!
//! ## 串行化
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex / `--test-threads=1` 双保险。
//!
//! ## clippy allow
//! 2026-09-16 PR-3：fixture helper（`insert_pool_part` / `insert_work_type` /
//!  `insert_worker` / `insert_l2_customer` 等）走 `pool_snowflake().lock()` 跨 .await
//! 持锁模式，与 common/ + prod::queue_api.rs 一致；`unused_imports` 是顶层
//!  `use SnowflakeIdGenerator` 仅作类型签名引用。
//! 2026-09-23 PR13 Phase D：`#![allow]` 已在 tests/production/mod.rs 集中豁免，
//! 本文件移除。
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! 通用 `login_manager` helper，统一走 `use hsh_erp_test_support::{...}` +
//! `bootstrap_as_manager()` + `load_production_fixture(&pool)`。保留：
//! - `login_manager_with_username`：本 sub-file 独享（每次用不同 username 登入）
//! - `insert_work_type` / `insert_worker` / `insert_customer_l2` / `insert_pool_part`：
//!   本 sub-file 独享的 raw SQL 构造（auto_allocate 域需要的
//!   `auto_allocate_limit` 字段是 prod::queue.rs 的 helper 没覆盖的，
//!   跨 binary 不通用）

use axum::http::StatusCode;
use serde_json::json;
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

/// MANAGER user：以新 username 登入 + MANAGER role（不影响 fixture 的 fx_part_manager）。
///
/// 本 sub-file 独享：每个测试常需要多次以不同 username 登入。
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

/// CLERK user：与 `login_manager_with_username` 同形、只换角色。
///
/// 2026-10-15：`queue.allocate` 自 Manager 独占放宽为 M,C 的**放宽留痕用例**
/// 需要 Clerk token。
async fn login_clerk_with_username(pool: &PgPool, username: &str) -> (axum::Router, String) {
    let uid = insert_user_with_password(pool, username, "changeme").await;
    add_role(pool, uid, "CLERK", None, None).await;
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
//  worker-pool auto_allocate fixture helpers（本 sub-file 独享，跨 binary 不迁移）
// ===========================================================================

/// 2026-10-15：旧的工时上限参数替换为 `auto_allocate_limit`（自动补料上限，
/// NULL = 未配置 ⇒ 服务端兜底 max_held_batches）。
async fn insert_work_type(
    pool: &PgPool,
    code: &str,
    name: &str,
    max_held_batches: Option<i32>,
    auto_allocate_limit: Option<i32>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_work_type (id, code, name, sort_order, version, \
         max_held_batches, auto_allocate_limit, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $5, $6, $6)",
        id,
        code,
        name,
        max_held_batches,
        auto_allocate_limit,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_work_type");
    id
}

/// 把 pool 批次直接改挂到 worker 名下（造「工人已持有」形态，省一份完整 fixture）。
/// 2026-10-15 新增，用于「auto 触顶」场景。
async fn assign_batch_to_worker(pool: &PgPool, batch_id: i64, worker_id: i64) {
    sqlx::query(
        "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = $2 WHERE id = $1",
    )
    .bind(batch_id)
    .bind(worker_id)
    .execute(pool)
    .await
    .expect("assign batch to worker");
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
    // 2026-09-16 PR-3 批次 step 化：prod::queue 候选池要求 part 已绑定工艺链
    // 且 batch 持有 current_process_step_id（worker.match 走 step.process_id）。
    // helper 现在多走两步：建链 → 建 step → INSERT part/batch。
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
    // 2026-09-30：prod::queue 候选池匹配改回
    // `pb.current_process_id = ANY(worker.process_ids)`（不再 JOIN
    // t_process_chain_step），helper 必须补该列否则批次对池隐身。
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

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: auto=2 / max=5 + 池 10 → `target=2`、`filled_count=2`；
/// 响应**不含** `mode` / `fill_ratio` 两个键（2026-10-15 破坏性 wire 变更）。
#[tokio::test]
async fn auto_allocate_targets_auto_limit() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "AC1").await;
    let proc = seed_process(&pool, "PROC-AC1", "工序").await;
    // auto=2 < max=5：自动路径的目标是 auto（不是 max）
    let wt = insert_work_type(&pool, "WT-AC1", "工种", Some(5), Some(2)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = insert_shelf(&pool, "PROD-AC1", "PROD-AC1", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let worker = insert_worker(&pool, "BC-AC1", "工", Some(wt)).await;
    for i in 0..10 {
        let sn = format!("AC1-P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, shelf, proc, 1).await;
    }

    let (app, token) = login_manager_with_username(&pool, "mgr_ac1").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto_allocate: {env}");
    let data = &env["data"];
    assert_eq!(data["process_id"], proc.to_string());
    let filled = data["filled"].as_array().expect("filled array");
    assert_eq!(filled.len(), 1, "应只 1 个 worker: {env}");
    assert_eq!(filled[0]["worker_id"], worker.to_string());
    assert_eq!(filled[0]["target"], 2, "auto=2 → target=2（不是 max=5）");
    assert_eq!(filled[0]["filled_count"], 2, "补到 auto 上限即停: {env}");
    assert_eq!(data["pool_empty"], false);

    // 2026-10-15：`mode` / `fill_ratio` 两个键已从响应删除（破坏性变更）
    let obj = data.as_object().expect("data 是 object");
    assert!(
        !obj.contains_key("mode"),
        "mode 键应已删除（2026-10-15 fill_ratio / mode 下线）: {env}"
    );
    assert!(
        !obj.contains_key("fill_ratio"),
        "fill_ratio 键应已删除（2026-10-15 fill_ratio / mode 下线）: {env}"
    );
}

/// 场景 2: auto=NULL → target 兜底 max（行为与引入自动上限前逐字一致）。
#[tokio::test]
async fn auto_allocate_falls_back_to_max_when_auto_null() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "AC2").await;
    let proc = seed_process(&pool, "PROC-AC2", "工序").await;
    let wt = insert_work_type(&pool, "WT-AC2", "工种", Some(5), None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = insert_shelf(&pool, "PROD-AC2", "PROD-AC2", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let worker = insert_worker(&pool, "BC-AC2", "工", Some(wt)).await;
    for i in 0..10 {
        let sn = format!("AC2-P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, shelf, proc, 1).await;
    }

    let (app, token) = login_manager_with_username(&pool, "mgr_ac2").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto_allocate: {env}");
    let filled = env["data"]["filled"].as_array().unwrap();
    assert_eq!(filled[0]["worker_id"], worker.to_string());
    assert_eq!(filled[0]["target"], 5, "auto=NULL → target 兜底 max=5");
    assert_eq!(filled[0]["filled_count"], 5, "池里 10 个 → 补满 5");
    assert_eq!(env["data"]["pool_empty"], false);
}

/// 场景 3: auto 触顶 → `skipped_reason=AUTO_LIMIT_REACHED`（**不是** `WORKER_FULL`）。
///
/// 判别力：工人持有 2、auto=2、max=5，池里还有 3 件货 —— 「池空」与「硬上限
/// 触顶」两个解释都被排除；真实成因只能是「自动上限到了，但手动路径仍可取」。
#[tokio::test]
async fn auto_allocate_reports_auto_limit_reached() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "AC3").await;
    let proc = seed_process(&pool, "PROC-AC3", "工序").await;
    let wt = insert_work_type(&pool, "WT-AC3", "工种", Some(5), Some(2)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = insert_shelf(&pool, "PROD-AC3", "PROD-AC3", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let worker = insert_worker(&pool, "BC-AC3", "工", Some(wt)).await;
    // 工人已持有 2 批（= auto 上限；仍低于 max=5）
    for i in 0..2 {
        let sn = format!("AC3-H-{:03}", i);
        let (_part, batch) = insert_pool_part(&pool, customer, &sn, shelf, proc, 1).await;
        assign_batch_to_worker(&pool, batch, worker).await;
    }
    // 池里还有 3 件货 —— 「池空」解释被排除
    for i in 0..3 {
        let sn = format!("AC3-P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, shelf, proc, 1).await;
    }

    let (app, token) = login_manager_with_username(&pool, "mgr_ac3").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto_allocate: {env}");
    let filled = env["data"]["filled"].as_array().unwrap();
    assert_eq!(filled[0]["target"], 2);
    assert_eq!(
        filled[0]["filled_count"], 0,
        "已到 auto 上限不应再抢: {env}"
    );
    assert_eq!(env["data"]["pool_empty"], true, "没抢够 ⇒ pool_empty=true");
    assert_eq!(
        filled[0]["skipped_reason"], "AUTO_LIMIT_REACHED",
        "池里有货、工人未触顶 max ⇒ 真实成因是自动上限，不能报 POOL_EMPTY / WORKER_FULL: {env}"
    );
}

/// 场景 4: `max_held_batches = NULL`（auto 也 NULL）→ `skipped_reason=MAX_BATCHES_NOT_SET`
/// 且 `target=0`；**批量端点不报错**（继续跑其余 worker 的语义由「返回 200」表达）。
#[tokio::test]
async fn auto_allocate_max_not_set_skips_worker() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "AC4").await;
    let proc = seed_process(&pool, "PROC-AC4", "工序").await;
    // max=NULL（未配置）⇒ 该工种不可自动补料
    let wt = insert_work_type(&pool, "WT-AC4", "工种", None, None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = insert_shelf(&pool, "PROD-AC4", "PROD-AC4", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let worker = insert_worker(&pool, "BC-AC4", "工", Some(wt)).await;
    // 池里放 1 件 —— 「池空」这个解释被排除
    insert_pool_part(&pool, customer, "AC4-P-001", shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "mgr_ac4").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "未配上限不报错、跳过该 worker: {env}");
    let filled = env["data"]["filled"].as_array().unwrap();
    assert_eq!(filled[0]["worker_id"], worker.to_string());
    assert_eq!(filled[0]["target"], 0, "未配上限 ⇒ target=0");
    assert_eq!(filled[0]["filled_count"], 0);
    assert_eq!(env["data"]["pool_empty"], true, "没抢够 ⇒ pool_empty=true");
    assert_eq!(
        filled[0]["skipped_reason"], "MAX_BATCHES_NOT_SET",
        "池里有货、工人持有 0 件 ⇒ 真实成因是「上限未配置」: {env}"
    );
}

/// 场景 5: 池空 → `pool_empty=true` 且 `skipped_reason=POOL_EMPTY`。
#[tokio::test]
async fn auto_allocate_pool_empty() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-AC5", "工序").await;
    let wt = insert_work_type(&pool, "WT-AC5", "工种", Some(5), None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = insert_shelf(&pool, "PROD-AC5", "PROD-AC5", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let _worker = insert_worker(&pool, "BC-AC5", "工", Some(wt)).await;
    // 不插任何 pool_part

    let (app, token) = login_manager_with_username(&pool, "mgr_ac5").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "empty pool 应 200: {env}");
    let filled = env["data"]["filled"].as_array().unwrap();
    assert_eq!(filled[0]["filled_count"], 0, "池空 → 0 件");
    assert_eq!(env["data"]["pool_empty"], true);
    assert_eq!(
        filled[0]["skipped_reason"], "POOL_EMPTY",
        "池里没货且工人没满 ⇒ POOL_EMPTY: {env}"
    );
}

/// 场景 6: process_id 不存在 → 20801。
#[tokio::test]
async fn auto_allocate_process_not_found() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let shelf = insert_shelf(&pool, "PROD-AC6", "PROD-AC6", "PRODUCTION").await;
    let nonexistent: i64 = 9_999_999_999_999;

    let (app, token) = login_manager_with_username(&pool, "mgr_ac6").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-allocate",
            Some(json!({
                "process_id": nonexistent.to_string(),
                "shelf_id": shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "无 process 应 404: {env}");
    assert_eq!(env["code"], 20801, "BIZ_PROCESS_NOT_FOUND: {env}");
}

// ===========================================================================
//  权限点闸门（2026-10-15 新增）
// ===========================================================================

/// 场景 7：auto-allocate 守卫迁 `queue.allocate` 后**放宽留痕** —— 原 Manager
/// 独占放宽为 M,C（用户定稿），Clerk 应能触发补料并真的补到上限。
#[tokio::test]
async fn auto_allocate_allows_clerk_after_allocation_widening() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "AC7").await;
    let proc = seed_process(&pool, "PROC-AC7", "工序").await;
    let wt = insert_work_type(&pool, "WT-AC7", "工种", Some(5), Some(2)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf = insert_shelf(&pool, "PROD-AC7", "PROD-AC7", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc).await;

    let worker = insert_worker(&pool, "BC-AC7", "工", Some(wt)).await;
    for i in 0..10 {
        let sn = format!("AC7-P-{:03}", i);
        insert_pool_part(&pool, customer, &sn, shelf, proc, 1).await;
    }

    let (app, token) = login_clerk_with_username(&pool, "clerk_ac7").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-allocate",
            Some(json!({
                "process_id": proc.to_string(),
                "shelf_id": shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "Clerk auto-allocate 应 200（放宽后）: {env}");
    let filled = env["data"]["filled"].as_array().expect("filled array");
    assert_eq!(filled.len(), 1);
    assert_eq!(filled[0]["worker_id"], worker.to_string());
    assert_eq!(filled[0]["filled_count"], 2, "Clerk 应能补到 auto 上限: {env}");
}
