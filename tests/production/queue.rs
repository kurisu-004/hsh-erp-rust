//! prod::queue 域端到端集成测试（原 worker_pool，2026-10-08 更名）
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
//!  17. state_without_shelf_id_returns_held_batches_and_empty_pool_count
//!      （2026-10-04 回归：`GET /pool/state` 的 `shelf_id` 降为可选后，缺省调用
//!      仍须返回完整持有视图，仅 `pool_count_by_process` 退化为空数组）
//!  18. move_worker_to_pool_picks_least_loaded_shelf
//!      （2026-10-10：撤回候选池的目标架按 `current_load / capacity` 升序自动选；
//!      `to` 侧无 `shelf_id`，断言落负载最低的架而非 display_order 最小的架）
//!  19. move_worker_to_pool_never_lands_on_unmapped_shelf
//!      （20507 退役后的端点级等价不变量：更靠前且完全空的**未映射**架也不能被选中）
//!  20. move_worker_to_pool_rejects_when_no_usable_shelf_for_process
//!      （2026-10-10 `current_holder_id` 写脏守卫（自动选架形态）：批次当前工序唯一
//!      映射的货架已软删 / 已停用 / 是品检架 ⇒ 选架候选为空 → 20508，批次不被写脏）
//!  21. move_worker_to_pool_no_process_still_lands_on_production_zone_shelf
//!      （同上，但 `current_process_id=NULL` ⇒ 选架不按工序筛候选的那条分支；
//!      断言 zone 谓词**仍然生效**，批次不会落到品检架）
//!  20. move_{pool_to_worker|worker_to_pool|worker_to_worker}_stale_version_returns_40901
//!      （2026-10-09 OCC：move 的 `version` 改为客户端必填，三个方向各一条，
//!      钉住「过期的看板快照真的会被挡住、批次不被写脏」）
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
pub(crate) async fn insert_user_with_password(
    pool: &PgPool,
    username: &str,
    plain_password: &str,
) -> i64 {
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
pub(crate) async fn add_role(
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
pub(crate) async fn seed_process(pool: &PgPool, code: &str, name: &str) -> i64 {
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
pub(crate) async fn link_work_type_to_process(pool: &PgPool, wt_id: i64, p_id: i64) {
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
pub(crate) async fn link_shelf_to_process(pool: &PgPool, s_id: i64, p_id: i64) {
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
pub(crate) async fn insert_shelf(pool: &PgPool, code: &str, name: &str, zone: &str) -> i64 {
    insert_shelf_state(pool, code, name, zone, true, false).await
}

/// 2026-10-04 新增：可指定 `is_active` / `deleted_at` 的 t_shelf 构造。
///
/// 供 `move_batch` WORKER→POOL 货架守卫的回归用例用（品检区 / 停用 / 软删三种形态）。
/// `insert_shelf` 改为委托本函数，避免同一目录出现两套货架 fixture 写法。
pub(crate) async fn insert_shelf_state(
    pool: &PgPool,
    code: &str,
    name: &str,
    zone: &str,
    is_active: bool,
    deleted: bool,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at, deleted_at) \
         VALUES ($1, $2, $3, $4, $5, 0, 0, $6, $6, \
         CASE WHEN $7 THEN $6::timestamp ELSE NULL END)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(zone)
    .bind(is_active)
    .bind(now)
    .bind(deleted)
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
pub(crate) async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ProductionFixture) {
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
pub(crate) async fn login_shelf_account(
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
pub(crate) async fn login_manager_with_username(
    pool: &PgPool,
    username: &str,
) -> (axum::Router, String) {
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

pub(crate) async fn insert_work_type(
    pool: &PgPool,
    code: &str,
    name: &str,
    max_held: Option<i32>,
) -> i64 {
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

pub(crate) async fn insert_worker(
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

pub(crate) async fn insert_customer_l2(pool: &PgPool, name: &str) -> i64 {
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
pub(crate) async fn insert_pool_part(
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
pub(crate) async fn insert_worker_held_part(
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

/// 给某条链**追加**一道 step，返回 step_id。
///
/// 2026-10-09：`insert_worker_held_part` / `insert_pool_part` 两个 fixture 只建
/// **一道** step（`sort_order = 1`）。而 worker-scan RETURNED 的新不变式要求目标
/// 工序登记在链内（链内找不到 ⇒ `20702`，见
/// `shared::batch::guards::optional_step_id` 的 doc），所以「RETURNED 推进到**另一
/// 道**工序」的用例必须先把那道工序补进链里。`sort_order` 用 20（稀疏）——
/// 读侧「下一道」按 `sort_order > 当前` 取，稀疏链才是它的判别性输入。
pub(crate) async fn append_chain_step(
    pool: &PgPool,
    chain_id: i64,
    process_id: i64,
    sort_order: i32,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let now = now_naive();
    let step_id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 30, 0, $5, 0, $5, 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(sort_order)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("append chain step");
    step_id
}

/// 取某个 part 当前绑定的链 id（前置断言用）。
pub(crate) async fn part_chain_id(pool: &PgPool, part_id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT process_chain_id FROM t_part WHERE id = $1")
        .bind(part_id)
        .fetch_one(pool)
        .await
        .expect("query t_part.process_chain_id")
}

/// 把 part_id 给定批次标为 worker 持有（针对 pool→worker 流转后的批次）。
pub(crate) async fn count_held_by_worker(pool: &PgPool, worker_id: i64) -> i64 {
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
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-002", worker, proc, 1, true).await;
    // 2026-10-10：清掉 step 指针，把批次压到「非顺应 ⇒ 用请求里的 `next_process_id`」
    // 那条分支。
    //
    // 为什么要清：fixture 造的是**单 step 链**，指针一致时 `chain_state == "TAIL"`
    // ⇒ RETURNED 会被「链尾自动送检」接管（那正是本用例**不**想测的路径 —— 本用例测
    // 「放回生产架 → refill」，链尾自动送检由
    // `worker_scan_returned_at_chain_tail_auto_sends_to_inspection` 专门覆盖）。
    // 清指针让 `is_pointer_consistent = false`，既绕开 TAIL 又落到显式分支。
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");
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
/// 2026-10-09：fixture 补一道链 step（`append_chain_step`）。RETURNED 的目标工序
/// 现在必须在链内 —— 链内找不到就以 `20702` 拒收（`optional_step_id` 的「真数据
/// 错误」判定），而本用例要验的是「推进 `current_process_id`」，前提就是目标工序
/// 在链内。断言本身逐字未动。
///
/// ⚠️ **本用例覆盖的是「非顺应 + 显式指定链内工序」这条分支**（2026-10-09 review
/// 第 1 轮订正）：补了 `append_chain_step(proc_c, 20)` 之后，批次形态是「指针指向
/// `proc_b` 的 step ∧ `current_process_id = proc_b`」= 顺应，且链上下一道恰好就是
/// `proc_c` —— 于是自动推进分支会成立，请求里的 `next_process_id` **被完全忽略**
/// （推进到 `proc_c` 只是因为链上下一道恰好是它，不是本用例传的值）。那会让
/// 「显式分支在集成层的唯一覆盖」消失，而断言逐字未动、照样全绿。
/// 修法：把批次指针置 `NULL` 造成非顺应（`is_pointer_consistent = false`），让
/// `next_process_id` 真正参与决策、step 由 `optional_step_id` 从链内解析。
/// 「顺应 + 自动推进」分支由场景 2e 的
/// `worker_scan_returned_advances_step_pointer_when_process_chain_is_consistent`
/// 覆盖（它刻意**不传** `next_process_id`），两条分支各有归属。
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
    let (held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-002B", worker, proc_b, 1, true).await;
    // 2026-10-09：把 RETURNED 的目标工序 proc_c 补进链里（fixture 只建一道 step）。
    // 新不变式下「链是有的，却没把目标工序登记进链内」是**真数据错误**（`20702`
    // `BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`），而本用例要验的是「RETURNED 推进
    // current_process_id」，前提必须是目标工序在链内。
    let chain_id = part_chain_id(&pool, held_part)
        .await
        .expect("fixture 应给该 part 绑了链");
    let proc_c_step = append_chain_step(&pool, chain_id, proc_c, 20).await;
    // ⚠️ 把批次指针置 NULL ⇒ 非顺应（`is_pointer_consistent = false`）。不做这一步，
    // 本用例会退化到自动推进分支、请求里的 `next_process_id` 被忽略（见 fn doc）。
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear batch step pointer to force explicit branch");

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

    // step 指针必须由**显式分支**的 `optional_step_id` 解析成链内那道 proc_c 的
    // step（自动推进分支取的是链上下一道 step，值相同但来源不同 —— 本用例钉的是
    // 显式分支，所以顺带钉住「step 由请求里那道工序在链内解析出来」）。
    let step_after: Option<i64> =
        sqlx::query_scalar("SELECT current_process_step_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("query current_process_step_id after RETURNED");
    assert_eq!(
        step_after,
        Some(proc_c_step),
        "显式分支应由 optional_step_id 把 step 解析成 proc_c 在链内那道（{proc_c_step}），\
         实际 {step_after:?}"
    );

    // 端点层：批次只应出现在 PROC-C 池，不应再出现在 PROC-B 池
    // （login_manager_with_username 会 INSERT t_user，只能调一次，后续复用 token）
    let (app, mgr) = login_manager_with_username(&pool, "admin_pool2b").await;
    let (sb, eb) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/prod/queue/processes/{proc_b}"),
            None,
            Some(&mgr),
        ),
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
        json_request(
            "GET",
            &format!("/prod/queue/processes/{proc_c}"),
            None,
            Some(&mgr),
        ),
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
/// 4. `current_process_step_id` 保留扫描前的值（else 分支解出的 step 是 `None`，
///    SQL 的 `COALESCE($5, current_process_step_id)` 必须保住原指针）
///
/// ## 2026-10-10：为什么本用例要造「指针漂移」而不是「清空指针」
/// 锚链解析会**回退**到批次的 step 指针所属链（`COALESCE(p.process_chain_id,
/// cur.chain_id)`），所以「part 无链 + 指针指向本链的 step」这条形态**仍然能解析出链**；
/// 而 fixture 造的是单 step 链 ⇒ 指针一致时 `chain_state == "TAIL"` ⇒ RETURNED 会被
/// 「链尾自动送检」接管。
///
/// 所以要落 else 分支（非顺应 ⇒ 用请求里的 `next_process_id`），必须让
/// `is_pointer_consistent = false`。**不能靠把指针清成 NULL**：那样断言 4 恒成立
/// （本来就是 NULL），`COALESCE` 保留语义就失去覆盖。改把指针指向**另一条链**上挂
/// 了**别的工序**的 step —— 锚链回退到那条链、按 `pb.current_process_id` 重定位落空、
/// `is_pointer_consistent` 为 false；`chain_state` 落 `NONE`（`cur2` 是内连接 LATERAL，
/// 0 行会丢掉整个 joined 行，取不到 TAIL 那条臂），链尾自动送检要求
/// `is_pointer_consistent && TAIL`，两个条件都不满足，于是落 else 分支并解出
/// `step = None`。
///
/// ## 断言 4 是承重的，不是护栏
/// `mark_batch_returned` 的 SQL 写的是
/// `current_process_step_id = COALESCE($5::bigint, current_process_step_id)`。把它改成
/// `= $5` 时本断言会红，而后果是每一次非顺应 RETURNED 都把批次链位置静默清空 ——
/// 所以断言 4 必须留着一个**非 NULL 的原值**可保。
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
    // 的旧 current_process_step_id（它属于 fixture 自建的那条单 step 链），断言 4
    // 用它当「写入不变式」的护栏。
    let (_held_part, held_batch, old_step) =
        insert_worker_held_part(&pool, customer, "H-002D", worker, proc_b, 1, false).await;
    // 2026-10-10：把批次指针改指到**另一条链**的 step 上（指针漂移），而不是清空它。
    //
    // 为什么必须造漂移而不是清空：本用例要测的是「非顺应 ⇒ 按前端指定的
    // `next_process_id` 推进」，而进入那条分支要求 `is_pointer_consistent = false`。
    // 两条路子都能造出非顺应，清空最省事 —— 但那样断言 4 就退化成「本来就是 NULL
    // 所以还是 NULL」，**恒成立**，`mark_batch_returned` 的
    // `current_process_step_id = COALESCE($5, current_process_step_id)` 这条不变式
    // 就没人守了（把它改成 `= $5` 测试也不会红，而后果是每一次非顺应 RETURNED 都把
    // 批次链位置静默清空）。
    //
    // 造漂移后本用例的形态：锚链解析回退到指针所属链（`COALESCE(p.process_chain_id,
    // cur.chain_id)`，本 part 无链 ⇒ 取指针所属的 foreign 链），而那条链里**没有**
    // `pb.current_process_id = proc_b` 这个工序 ⇒ 按工序重定位落空 ⇒
    // `current_step_id` 为 NULL ⇒ `is_pointer_consistent = false`。
    //
    // ⚠️ 此时 `chain_state` 落 **`NONE`** 而不是 `TAIL`：`cur2` 在
    // `CHAIN_POSITION_LATERAL_SQL` 里是 `JOIN LATERAL (…) cur2 ON TRUE`（**内**
    // 连接），重定位 0 行会把整个 joined 行丢掉，于是取不到 `nsp.id IS NULL ⇒ TAIL`
    // 那条臂，只能由外层 `COALESCE(nx.chain_state, 'NONE')` 兜底成 `NONE`。
    // 落 `NONE` 同样不进「链尾自动送检」（那条要求 `is_pointer_consistent && TAIL`，
    // 两个条件都不满足），所以照样落 else 分支 ⇒ `step_id_opt = None` 绑进 SQL，
    // 断言 4 真正钉住 COALESCE 保留语义。
    //
    // ⚠️ foreign 链的那道 step **必须**挂 `proc_c`（≠ 批次当前工序 `proc_b`）：若挂上
    // `proc_b`，按工序重定位会正好命中它，`current_step_id == 指针` ⇒ 指针反而变成
    // 一致的，`chain_state` 与 `is_pointer_consistent` 同时为真 ⇒ 请求被「链尾自动
    // 送检」接管，本用例就测不到 else 分支了。
    let foreign_chain = {
        use hsh_erp_test_support::shared_test_snowflake;
        shared_test_snowflake().next_id()
    };
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, $2, 0, now(), 0, now(), 0)",
    )
    .bind(foreign_chain)
    .bind("chain-H-002D-foreign")
    .execute(&pool)
    .await
    .expect("insert foreign chain");
    let foreign_step = append_chain_step(&pool, foreign_chain, proc_c, 10).await;
    assert_ne!(
        foreign_step, old_step,
        "foreign step 必须与 fixture 原指针不同，否则造不出指针漂移"
    );
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = $2 WHERE id = $1")
        .bind(held_batch)
        .bind(foreign_step)
        .execute(&pool)
        .await
        .expect("point batch at a foreign chain step");

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
        Some(foreign_step),
        "else 分支解出的 step 是 None，RETURNED 后 SQL 的 COALESCE 必须保住扫描前的指针 \
         {foreign_step}（把 COALESCE 改成直接写 $5 会让这里变 None），实际 {:?}",
        after.1
    );
    assert_ne!(
        foreign_step, old_step,
        "指针漂移的前提：foreign step 必须不同于 fixture 原指针"
    );
}

/// 造一个「**已绑链的 PENDING 批次**」，链内 `process_ids` 按数组顺序占
/// `sort_order` 10 / 20 / 30…（稀疏）。返回 `(part_id, batch_id, step_ids)`。
///
/// 2026-10-09 新增：`insert_pool_part` 造的是「**已下发**且在池」的批次
/// （`IN_PROCESS` + `PRODUCTION_SHELF` + 指针已落链首），而「dispatch 按链首
/// 下发」这条不变式的输入恰恰是 dispatch **之前**的形态 —— PENDING、指针 NULL。
/// 两者不能互相顶替，故单开一个 helper。
pub(crate) async fn insert_pending_part_with_chain(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
    process_ids: &[i64],
) -> (i64, i64, Vec<i64>) {
    use hsh_erp_rust::infra::clock::now_naive;
    let now = now_naive();
    let today = now.date();
    let chain_id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, $2, 0, $3, 0, $3, 0)",
    )
    .bind(chain_id)
    .bind(format!("chain-{serial_no}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert chain");
    let mut step_ids = Vec::new();
    for (i, pid) in process_ids.iter().enumerate() {
        step_ids.push(append_chain_step(pool, chain_id, *pid, (i as i32 + 1) * 10).await);
    }
    let part_id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, \
         request_date, planned_delivery_date, system_delivery_date, status, \
         is_urgent, next_process_id, customer_id, quantity, version, created_at, \
         updated_at, process_chain_id) \
         VALUES ($1, $2, 'pending-chain-item', 'D-PENDCH', $2, $3, $3, $3, 'PENDING', \
         false, NULL, $4, 1, 0, $5, $5, $6)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(today)
    .bind(customer_id)
    .bind(now)
    .bind(chain_id)
    .execute(pool)
    .await
    .expect("insert t_part with chain");
    let batch_id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    // 初始批次：PENDING / location NULL / 指针与工序均 NULL（dispatch 的真实输入）
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) VALUES ($1, $2, 1, 1, 'PENDING', 0, $3, $3)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch PENDING");
    (part_id, batch_id, step_ids)
}

/// 场景 2e（2026-10-09 新增）：**顺应工序**时 RETURNED 自动按链推进 —— 请求体
/// **不带** `next_process_id`，后端自己查链上下一道，两列同时推进。
///
/// ## 为什么这条用例是端到端的
/// 它把三条写点串成一条真实主干流：
/// 1. `POST /prod/queue/dispatch` —— 落**链首** step（`current_process_id` =
///    链首工序、`current_process_step_id` = 链首 step）；
/// 2. `POST /prod/queue/move` POOL→WORKER —— 批次压到工人手上（工序与指针都不动）；
/// 3. `POST /prod/batches/worker-scan` RETURNED —— **不带** `next_process_id`。
///
/// 只有第 1 步把指针落到链首 step，第 3 步的 `ChainPosition::is_pointer_consistent`
/// 才会为真、自动推进分支才可达。缺任一步本用例都会退化成「要求前端显式指定」
/// 那条分支（返回 40001）。
///
/// ## 断言
/// 推进后 `current_process_id` = 第二道工序、`current_process_step_id` =
/// 第二道 step（**两列一起动**，这正是 `mark_batch_returned` 本轮恢复写 step 的
/// 目的）。指针停在链首 step 而工序推进了那种「两列不同步」的形态，正是本次要消灭
/// 的状态。
#[tokio::test]
async fn worker_scan_returned_advances_step_pointer_when_process_chain_is_consistent() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2E").await;
    // 链 = [PROC-2E1(10), PROC-2E2(20)]；工种只映射第一道（否则 refill 会把刚
    // 归还的批次又抢回工人，干扰「落进第二道池」的断言）
    let proc_1 = seed_process(&pool, "PROC-2E1", "工序2E1").await;
    let proc_2 = seed_process(&pool, "PROC-2E2", "工序2E2").await;
    let wt = insert_work_type(&pool, "WT-2E", "工种2E", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc_1).await;
    // 同一货架映射两道工序：dispatch 按链首解析货架、RETURNED 按推导出的第二道
    // 校验货架映射，两者都要过各自的货架守卫
    let prod_shelf = insert_shelf(&pool, "PROD-2E", "PROD-2E", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_1).await;
    link_shelf_to_process(&pool, prod_shelf, proc_2).await;

    let worker = insert_worker(&pool, "BC002E", "工2E", Some(wt)).await;
    let (_part_id, batch_id, step_ids) =
        insert_pending_part_with_chain(&pool, customer, "P-002E", &[proc_1, proc_2]).await;
    let head_step = step_ids[0];
    let second_step = step_ids[1];

    // 前置：dispatch 之前指针与工序都必须是 NULL（真实输入形态）
    let before: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("query pending batch");
    assert_eq!(
        before,
        (None, None),
        "前置：PENDING 批次的工序与链内指针都应为 NULL"
    );

    let (app, mgr) = login_manager_with_username(&pool, "admin_pool2e").await;

    // 1. dispatch —— 落链首 step（请求里刻意传第二道工序，它必须被忽略）
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": proc_2.to_string(),
                }]
            })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "dispatch: {env1}");
    let after_dispatch: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("query after dispatch");
    assert_eq!(
        after_dispatch,
        (Some(proc_1), Some(head_step)),
        "dispatch 应落链首工序 + 链首 step（≠ 请求里的 proc_2）: {env1}"
    );

    // 2. move POOL → WORKER（工序不动，指针不动）
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/queue/move",
            Some(json!({
                "batch_id": batch_id.to_string(),
                // dispatch 刚把 version 从 0 推到 1
                "version": 1,
                "from": { "kind": "POOL",   "shelf_id": prod_shelf.to_string() },
                "to":   { "kind": "WORKER", "worker_id": worker.to_string() },
            })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "move POOL→WORKER: {env2}");

    // 3. worker-scan RETURNED —— **不带** next_process_id
    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2e", &[prod_shelf]).await;
    let (s3, env3) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "P-002E",
                "badge_code": "BC002E",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s3,
        StatusCode::OK,
        "顺应工序时不该要求前端传 next_process_id: {env3}"
    );

    // 断言：两列一起推进到第二道
    let after: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("query after RETURNED");
    assert_eq!(after.0, Some(proc_2), "RETURNED 应自动推进到链上下一道工序");
    assert_eq!(
        after.1,
        Some(second_step),
        "RETURNED 应把链内位置指针一起推进到下一 step（两列必须同步）"
    );
    assert_ne!(
        after.1,
        Some(head_step),
        "指针不能停在链首 step —— 那样下次放回就会按错误位置推导下一道"
    );
}

/// 场景 2f（2026-10-09 新增，**行为变更**）：非顺应工序 + 请求体不带
/// `next_process_id` ⇒ `40001 VALIDATION_ERROR`，文案点明成因。
///
/// 「非顺应」的成因共四种（无链 / 链已软删 / 指针漂移 / 链内工序重复）与链尾，
/// 全落这一个分支。这里用场景 2d 的 fixture 形态（`with_chain=false`：part 无链、
/// 批次带一个孤儿 step 指针）—— 指针非 NULL 但它所属的链不是锚链，判据同样为
/// false。
#[tokio::test]
async fn worker_scan_returned_requires_next_process_id_when_not_consistent() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2F").await;
    let proc_b = seed_process(&pool, "PROC-2F1", "工序2F1").await;
    let proc_c = seed_process(&pool, "PROC-2F2", "工序2F2").await;
    let wt = insert_work_type(&pool, "WT-2F", "工种2F", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc_b).await;
    let prod_shelf = insert_shelf(&pool, "PROD-2F", "PROD-2F", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_c).await;

    let worker = insert_worker(&pool, "BC002F", "工2F", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-002F", worker, proc_b, 1, false).await;
    // 2026-10-10：把 step 指针清成 NULL，造出**真的**非顺应形态。
    //
    // fixture 造的链是单 step 链，而锚链解析会回退到 `cur.chain_id`（批次的 step 指针
    // 所属链）—— 于是 `with_chain=false` 并不足以让本批次落到「非顺应」：锚链仍能解析、
    // 指针仍一致、`chain_state` 落 `TAIL`。指针清空后 `is_pointer_consistent = false`，
    // 三条闸门（非顺应 / 指针漂移）都成立，本用例才真正测到它声称测的那条分支。
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2f", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "H-002F",
                "badge_code": "BC002F",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "非顺应工序缺参应 422: {env}"
    );
    assert_eq!(env["code"], 40001, "应为 VALIDATION_ERROR: {env}");
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("next_process_id") && msg.contains("非顺应工序"),
        "文案要同时点名「缺哪个字段」与「为什么必须填」，否则运营只会看到 422 去查前端: {env}"
    );

    // 批次不该被写脏
    let (location, holder): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT location, current_holder_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("query batch after rejected scan");
    assert_eq!(
        location.as_deref(),
        Some("WORKER"),
        "拒收时批次仍应留在工人手上"
    );
    assert_eq!(holder, Some(worker));
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
            "/prod/queue/refill",
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
            "/prod/queue/refill",
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
            "/prod/queue/refill",
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
            "/prod/queue/refill",
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
            "/prod/queue/refill",
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
            "/prod/queue/refill",
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
            "/prod/queue/refill",
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
/// 2026-09-30 之前：原 `admin_remove` 端点。重构后走统一 `POST /prod/queue/move`
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
            "/prod/queue/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                // 2026-10-09：move 的 OCC 锚必填；两个 fixture helper 建批时 version 写死 0
                "version": 0,
                "from": { "kind": "WORKER", "worker_id": worker.to_string() },
                "to":   { "kind": "POOL" },
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
            "/prod/queue/move",
            Some(json!({
                "batch_id": pool_batch.to_string(),
                // 2026-10-09：move 的 OCC 锚必填；fixture 建批时 version 写死 0
                "version": 0,
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
            "/prod/queue/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                // 2026-10-09：move 的 OCC 锚必填；两个 fixture helper 建批时 version 写死 0
                "version": 0,
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
            "/prod/queue/move",
            Some(json!({
                "batch_id": pool_batch.to_string(),
                // 2026-10-09：move 的 OCC 锚必填；fixture 建批时 version 写死 0
                "version": 0,
                // from 谎报成 WORKER（实际在 POOL），期望 40904
                "from": { "kind": "WORKER", "worker_id": "999999999" },
                "to":   { "kind": "POOL" },
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
            "/prod/queue/move",
            Some(json!({
                "batch_id": held_batch_src.to_string(),
                // 2026-10-09：move 的 OCC 锚必填；fixture 建批时 version 写死 0
                "version": 0,
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
            "/prod/queue/move",
            Some(json!({
                "batch_id": batch.to_string(),
                // 2026-10-09：move 的 OCC 锚必填；fixture 建批时 version 写死 0
                "version": 0,
                "from": { "kind": "POOL", "shelf_id": prod_shelf.to_string() },
                "to":   { "kind": "POOL" },
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
            "/prod/queue/refill",
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
            "/prod/queue/refill",
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
pub(crate) async fn insert_l2_customer(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
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

/// 场景 H1: happy path —— `GET /prod/queue/processes/{process_id}` 返回
/// `process` 元数据 + `workers[]`（含 max_held / current_held / capacity_remaining）
/// + `items[]` 候选批次。排序：system_delivery_date ASC NULLS LAST → is_urgent DESC → id ASC。
#[tokio::test]
async fn board_process_detail_happy() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    // L1 + L2 客户
    let l1 = insert_customer_l2(&pool, "L1-NAME").await;
    let l2 = insert_l2_customer(&pool, "L2-NAME", l1).await;

    let proc = seed_process(&pool, "PROC-PBP", "工序PBP").await;
    let wt_a = insert_work_type(&pool, "WT-PBP-A", "工种A", Some(3)).await;
    // 工种B 未设上限（max_held_batches = NULL）⇒ 该 worker 的 max_held 退化 0
    let wt_b = insert_work_type(&pool, "WT-PBP-B", "工种B", None).await;
    link_work_type_to_process(&pool, wt_a, proc).await;
    link_work_type_to_process(&pool, wt_b, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-PBP", "PROD-PBP", "PRODUCTION").await;

    let w_a = insert_worker(&pool, "BC-PBP-A", "工A", Some(wt_a)).await;
    let _w_b = insert_worker(&pool, "BC-PBP-B", "工B", Some(wt_b)).await;

    // 2 批次：urgent 在前（同 system_delivery_date 时 is_urgent DESC 排序在前）。
    // `insert_pool_part` 把 is_urgent 硬编码为 false —— 加急单用 UPDATE 翻成 true。
    // 用 `sqlx::query`（runtime）而非 `query!` 避免动 `.sqlx` cache。
    let (p_urgent, _b_urgent) = insert_pool_part(&pool, l2, "U-001", prod_shelf, proc, 2).await;
    let (_p_normal, _b_normal) = insert_pool_part(&pool, l2, "N-001", prod_shelf, proc, 5).await;
    sqlx::query("UPDATE t_part SET is_urgent = true WHERE id = $1")
        .bind(p_urgent)
        .execute(&pool)
        .await
        .expect("mark U-001 urgent");

    // 工A 持有 1 批（验证 held_batches 与容量计算）
    insert_worker_held_part(&pool, l2, "H-PBP", w_a, proc, 1, true).await;

    let (app, token) = login_manager_with_username(&pool, "admin_pbp").await;
    // 注意：`test_support::test_app` 用 `v2_router()`（不带 `/api/v2` nest），
    // 所以测试 URI 是 `/prod/queue/...` 而不是 `/api/v2/prod/queue/...`。
    let uri = format!("/prod/queue/processes/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "board_process_detail happy: {env}");
    assert_eq!(env["code"], 0, "code 应 0: {env}");

    // 工序元数据收在 process 子对象里（不再平铺）
    let proc_meta = &env["data"]["process"];
    assert_eq!(proc_meta["process_id"], proc.to_string());
    assert_eq!(proc_meta["process_code"], "PROC-PBP");
    assert_eq!(proc_meta["process_name"], "工序PBP");
    // ts 存在且是 RFC3339 带 +08:00
    assert!(
        env["data"]["ts"].as_str().unwrap().ends_with("+08:00"),
        "ts 应带 +08:00 偏移: {env}"
    );

    let workers = env["data"]["workers"].as_array().expect("workers array");
    assert_eq!(workers.len(), 2, "workers 应 2 个: {env}");
    // max_held 直接挂到 worker 上（不再有独立的 work_types[] 数组）
    let wa = workers
        .iter()
        .find(|w| w["worker_id"].as_str() == Some(w_a.to_string().as_str()))
        .expect("工A 应在 workers 里");
    assert_eq!(wa["max_held"], 3, "工A max_held 应为工种上限 3: {env}");
    assert_eq!(wa["current_held"], 1, "工A 持有 1 批: {env}");
    assert_eq!(
        wa["capacity_remaining"], 2,
        "capacity_remaining 应为 3-1=2: {env}"
    );
    assert_eq!(wa["work_type_code"], "WT-PBP-A");
    assert_eq!(wa["badge_code"], "BC-PBP-A");
    assert_eq!(wa["held_batches"].as_array().unwrap().len(), 1, "{env}");
    // 工种未设 max_held_batches ⇒ 退化为 0（不是 null）
    let wb = workers
        .iter()
        .find(|w| w["work_type_code"] == "WT-PBP-B")
        .expect("工B 应在 workers 里");
    assert_eq!(wb["max_held"], 0, "未设上限应退化为 0: {env}");
    assert_eq!(wb["capacity_remaining"], 0, "capacity 不为负: {env}");

    assert_eq!(env["data"]["total"], 2, "total 应 2: {env}");
    let items = env["data"]["items"].as_array().expect("items array");
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
    // customer_path 已删（前端自行拼 L1 / L2），客户两级名仍在
    assert_eq!(items[0]["customer_name"], "L2-NAME", "{env}");
    assert_eq!(items[0]["parent_customer_name"], "L1-NAME", "{env}");
    assert!(
        items[0].get("customer_path").is_none(),
        "customer_path 应已删除: {env}"
    );
    // shelf 元数据存在（POOL→WORKER move 的 from.shelf_id 数据源）
    assert_eq!(items[0]["shelf_id"], prod_shelf.to_string());
    assert_eq!(items[0]["shelf_code"], "PROD-PBP");
    assert_eq!(items[0]["shelf_name"], "PROD-PBP");
}

/// 场景 H2: 不存在的 process_id → 20801 BIZ_PROCESS_NOT_FOUND + 404
#[tokio::test]
async fn board_process_detail_process_not_found() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-NF", "工序NF").await;
    let _wt = insert_work_type(&pool, "WT-NF", "工种NF", Some(3)).await;
    link_work_type_to_process(&pool, _wt, proc).await;
    // 一个不存在的 snowflake-style id（远大于实际生成）
    let nonexistent_id: i64 = 9_999_999_999_999;

    let (app, token) = login_manager_with_username(&pool, "admin_nf").await;
    let uri = format!("/prod/queue/processes/{nonexistent_id}");
    let (s, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "不存在 process 应 404: {env}");
    assert_eq!(env["code"], 20801, "BIZ_PROCESS_NOT_FOUND: {env}");
}

/// 场景 H3: ShelfAccount 角色 → 40300 FORBIDDEN（service 守卫：Manager/Clerk/Inspector only）
#[tokio::test]
async fn board_process_detail_forbidden_for_shelf_account() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-FB", "工序FB").await;
    let wt = insert_work_type(&pool, "WT-FB", "工种FB", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-FB", "PROD-FB", "PRODUCTION").await;

    // ShelfAccount 绑一个 shelf（scope 必须给才能登录；调用端点时仍会被 service 拒绝）
    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "shelf_user_fb", &[prod_shelf]).await;
    let uri = format!("/prod/queue/processes/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "ShelfAccount 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// 场景 H4: process 存在但无候选批次 → total=0, items=[]，元数据正常返回
#[tokio::test]
async fn board_process_detail_no_candidates_when_no_batch() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-EMPTY", "空工序").await;
    let wt = insert_work_type(&pool, "WT-EMPTY", "空工种", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let _w = insert_worker(&pool, "BC-EMPTY", "空工人", Some(wt)).await;

    let (app, token) = login_manager_with_username(&pool, "admin_empty").await;
    let uri = format!("/prod/queue/processes/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "无 batch 应 200: {env}");
    assert_eq!(env["code"], 0, "code 应 0: {env}");

    let data = &env["data"];
    assert_eq!(data["process"]["process_id"], proc.to_string());
    assert_eq!(data["process"]["process_code"], "PROC-EMPTY");
    assert_eq!(data["process"]["process_name"], "空工序");
    assert_eq!(data["total"], 0, "total 应 0: {env}");
    let items = data["items"].as_array().expect("items array");
    assert_eq!(items.len(), 0, "items 应 []: {env}");
    // workers 仍返回（该工种有 1 个 active 工人），held_batches 为空
    let workers = data["workers"].as_array().expect("workers array");
    assert_eq!(workers.len(), 1, "workers 应 1 个: {env}");
    assert_eq!(
        workers[0]["max_held"], 3,
        "max_held 直接挂 worker 上: {env}"
    );
    assert_eq!(workers[0]["current_held"], 0, "{env}");
    assert_eq!(
        workers[0]["held_batches"].as_array().unwrap().len(),
        0,
        "{env}"
    );
    // work_types[] 数组已删（前端零消费，max_held 改由 workers[].max_held 表达）
    assert!(
        data.get("work_types").is_none(),
        "work_types[] 应已删除: {env}"
    );
}

// ===========================================================================
// 2026-09-30 重构：原 `admin/worker-pool/assign` 4 个端点已删除，被
// `POST /api/v2/prod/queue/move` 取代。覆盖：
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
pub(crate) async fn seed_g_code_for_part(pool: &PgPool, part_id: i64) -> i64 {
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
            "/prod/queue/refill",
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
    let uri = format!("/prod/queue/processes/{proc}");
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

/// `GET /prod/queue/processes/{process_id}` 应在
/// `workers[].held_batches[*].has_cnc_program` 透传实际值。
///
/// 2026-10-08：`GET /pool/state` 端点已删（原设计要前端每 worker 发一次请求 = N+1），
/// 持有批次改由工序板端点一次返回。字段口径不变 —— 前端「已编程」tag 渲染依赖本字段，
/// 后端必须真实返回 EXISTS(G_CODE) 值，否则 zod strip 模式下前端会因必填字段缺失
/// 直接抛校验异常。
///
/// 场景：worker 持有 2 个 batch（A 有 G_CODE，B 无），断言 `has_cnc_program`
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

    let (app, token) = login_manager_with_username(&pool, "admin_cnc_held").await;
    let uri = format!("/prod/queue/processes/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None::<Value>, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "board process detail: {env}");
    // 该工序只有 1 个 worker（BC-CNC-HELD），其 held_batches 应有 2 条
    let workers = env["data"]["workers"].as_array().expect("workers array");
    assert_eq!(workers.len(), 1, "应 1 个 worker: {env}");
    assert_eq!(workers[0]["current_held"], 2, "current_held 应 2: {env}");
    let held = workers[0]["held_batches"]
        .as_array()
        .expect("held_batches array");
    assert_eq!(held.len(), 2, "应 2 个 held batch: {env}");
    // 持有态不该再返回 shelf_code（current_holder_id 是 worker，t_shelf JOIN 恒不命中）
    assert!(
        held[0].get("shelf_code").is_none(),
        "held_batches[].shelf_code 应已删除: {env}"
    );
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
//  2026-10-10 move WORKER → POOL 的目标货架守卫（改自动选架后的形态）
// ===========================================================================
//
// 背景：`move_batch` 的 WORKER→POOL 分支把选出来的货架写进
// `t_part_batch.current_holder_id` 并把 `location` 翻成 `PRODUCTION_SHELF`。
// 报工台取件页数据源（`part::service::phase1::work_type` pickable-by-work-type）硬限定
// `JOIN t_shelf sh ON sh.id = b.current_holder_id AND sh.is_active = true
//   AND sh.zone = 'PRODUCTION'`，故落到品检架 / 停用架 / 已软删架上的批次永远不会被
// 工人领到，也不报错 —— 静默漏件。
//
// 2026-10-10：目标架不再由调用方传（`to.kind = "POOL"` 无字段），改由
// `shared::shelf::select::pick_least_loaded` 按批次当前工序选。这三条谓词
// （未软删 / `is_active` / `zone='PRODUCTION'`）变成了选架**候选集**的一部分，
// 选架层自己的单测 `shared::shelf::select::tests::
// inactive_and_soft_deleted_shelves_are_excluded` 已锁住谓词本身；本文件锁的是
// **端到端后果**：这些架一个都选不中时请求被拒、且批次不被写脏。

/// WORKER→POOL 的目标架由**负载**决定（2026-10-10 自动选架的核心行为）。
///
/// 两个映射架的 `display_order` 与负载排成**相反**的次序（A 在前但更满、B 在后但更空），
/// 于是「按物理顺序取第一个」与「按负载取最空」两种口径被分开：断言落 B。
#[tokio::test]
async fn move_worker_to_pool_picks_least_loaded_shelf() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "MPICKS").await;
    let proc = seed_process(&pool, "PROC-MPICK", "工序MPICK").await;
    let wt = insert_work_type(&pool, "WT-MPICK", "工种MPICK", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-MPICK", "工MPICK", Some(wt)).await;

    let shelf_a = insert_shelf(&pool, "MPICK-A", "MPICK架A", "PRODUCTION").await;
    let shelf_b = insert_shelf(&pool, "MPICK-B", "MPICK架B", "PRODUCTION").await;
    for shelf in [shelf_a, shelf_b] {
        sqlx::query("UPDATE t_shelf SET capacity = 100, display_order = $2 WHERE id = $1")
            .bind(shelf)
            .bind(if shelf == shelf_a { 0_i32 } else { 1 })
            .execute(&pool)
            .await
            .expect("set capacity/display_order");
        link_shelf_to_process(&pool, shelf, proc).await;
    }
    // 在架负载：A 80 件、B 20 件（`SUM(quantity)` 件数口径）⇒ 比例 80% / 20%
    let (_pa, _ba) = insert_pool_part(&pool, customer, "MPICK-LA", shelf_a, proc, 80).await;
    let (_pb, _bb) = insert_pool_part(&pool, customer, "MPICK-LB", shelf_b, proc, 20).await;

    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-MPICK", worker, proc, 1, true).await;

    let (app, token) = login_manager_with_username(&pool, "admin-mpick").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                "version": 0,
                "from": { "kind": "WORKER", "worker_id": worker.to_string() },
                // 2026-10-10：to 侧无 shelf_id —— 目标架由服务端按负载选
                "to":   { "kind": "POOL" },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "move WORKER→POOL: {env}");
    assert_eq!(
        env["data"]["new_holder_id"],
        shelf_b.to_string(),
        "应落负载比例最低的架（20%），不是 display_order 最小的 A（80%）: {env}"
    );
}

/// WORKER→POOL 绝不会把批次落到**未映射该工序**的架上 —— 20507 那条守卫退役后的
/// 端点级等价不变量。
///
/// 构造：同 zone 有三个架，其中 `UNMAPPED` 既 `display_order` 最靠前（0）又**完全空**
/// （负载 0）。若端点只按 zone + 负载选、不看映射，它会选 `UNMAPPED`（0 件 < 80 件）；
/// 正确的行为是映射谓词把它挡在候选集外 ⇒ 落 `MAPPED`。
///
/// 选架层已有单元级护栏（`shared::shelf::select::tests::
/// process_id_restricts_candidates_to_mapped_shelves`）锁那个 `EXISTS` 谓词；本用例锁
/// 的是「端点真的经由选架」—— 若哪天有人绕过 `pick_least_loaded` 直接写
/// `current_holder_id`，这里会红。
#[tokio::test]
async fn move_worker_to_pool_never_lands_on_unmapped_shelf() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "MPUNMAP").await;
    let proc = seed_process(&pool, "PROC-MPUM", "工序MPUM").await;
    let proc_other = seed_process(&pool, "PROC-MPUM2", "工序MPUM2").await;
    let wt = insert_work_type(&pool, "WT-MPUM", "工种MPUM", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-MPUM", "工MPUM", Some(wt)).await;

    // 空架、未映射本工序、display_order 最靠前 —— 「只看 zone+负载」的实现会选它
    let unmapped = insert_shelf(&pool, "MPUM-UNMAP", "MPUM未映射架", "PRODUCTION").await;
    sqlx::query("UPDATE t_shelf SET capacity = 100, display_order = 0 WHERE id = $1")
        .bind(unmapped)
        .execute(&pool)
        .await
        .expect("set unmapped shelf");
    link_shelf_to_process(&pool, unmapped, proc_other).await;

    let mapped = insert_shelf(&pool, "MPUM-MAP", "MPUM已映射架", "PRODUCTION").await;
    sqlx::query("UPDATE t_shelf SET capacity = 100, display_order = 1 WHERE id = $1")
        .bind(mapped)
        .execute(&pool)
        .await
        .expect("set mapped shelf");
    link_shelf_to_process(&pool, mapped, proc).await;
    // mapped 装 80 件（比例 80%）—— 仍必须胜过空的 unmapped（0 件）
    let (_pa, _ba) = insert_pool_part(&pool, customer, "MPUM-LOAD", mapped, proc, 80).await;

    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-MPUM", worker, proc, 1, true).await;

    let (app, token) = login_manager_with_username(&pool, "admin-mpum").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                "version": 0,
                "from": { "kind": "WORKER", "worker_id": worker.to_string() },
                "to":   { "kind": "POOL" },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "move WORKER→POOL: {env}");
    assert_eq!(
        env["data"]["new_holder_id"],
        mapped.to_string(),
        "必须落映射了本工序的架；未映射的架哪怕更空也不该被选: {env}"
    );
    assert_ne!(
        env["data"]["new_holder_id"],
        unmapped.to_string(),
        "绝不能落到未映射 {proc} 的架上（20507 退役后的等价不变量）"
    );

    let holder: Option<i64> =
        sqlx::query_scalar("SELECT current_holder_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("read holder");
    assert_eq!(holder, Some(mapped));
}

/// 该工序唯一映射的货架不可用（已软删 / 已停用 / 品检区）→ 20508 且批次不被写脏。
#[tokio::test]
async fn move_worker_to_pool_rejects_when_no_usable_shelf_for_process() {
    // (短标, 说明, zone, is_active, deleted)
    // 「短标」只进 serial_no / shelf code 等有长度上限的列（`t_part.serial_no` 是
    // varchar(15)），长描述只进断言消息。
    let cases: [(&str, &str, &str, bool, bool); 3] = [
        ("DEL", "soft-deleted", "PRODUCTION", true, true),
        ("INACT", "inactive", "PRODUCTION", false, false),
        ("INSP", "inspection-zone", "INSPECTION", true, false),
    ];

    for (tag, name, zone, is_active, deleted) in cases {
        let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
        let customer = insert_customer_l2(&pool, "MPG").await;
        let proc = seed_process(&pool, "PROC-MPG", "工序MPG").await;
        let wt = insert_work_type(&pool, "WT-MPG", "工种MPG", Some(5)).await;
        link_work_type_to_process(&pool, wt, proc).await;
        let worker =
            insert_worker(&pool, &format!("BC-{tag}"), &format!("工{tag}"), Some(wt)).await;

        // 该工序**唯一**的映射货架，按用例指定状态。选了它就必然落选 ⇒ 候选为空。
        let bad_shelf = insert_shelf_state(
            &pool,
            &format!("SH-{tag}"),
            &format!("SH-{tag}"),
            zone,
            is_active,
            deleted,
        )
        .await;
        link_shelf_to_process(&pool, bad_shelf, proc).await;

        let (_part, held_batch, _step) =
            insert_worker_held_part(&pool, customer, &format!("H-{tag}"), worker, proc, 1, true)
                .await;

        let (app, token) = login_manager_with_username(&pool, "admin-mpg").await;
        let (s, env) = send(
            app,
            json_request(
                "POST",
                "/prod/queue/move",
                Some(json!({
                    "batch_id": held_batch.to_string(),
                    // 2026-10-09：move 的 OCC 锚必填；fixture 建批时 version 写死 0
                    "version": 0,
                    "from": { "kind": "WORKER", "worker_id": worker.to_string() },
                    // 2026-10-10：to 侧不再有 shelf_id（目标架自动选）
                    "to":   { "kind": "POOL" },
                })),
                Some(&token),
            ),
        )
        .await;
        // 20508 落在「资源缺失」段 → HTTP 404（`error.rs::status_from_code` 的既有映射）
        assert_eq!(s, StatusCode::NOT_FOUND, "{name}: 应拒收: {env}");
        assert_eq!(
            env["code"].as_i64().unwrap(),
            20508,
            "{name}: 无可用生产货架应 20508 BIZ_SHELF_PROCESS_NOT_FOUND: {env}"
        );

        // 批次未被写脏：仍在 worker 手上（location/holder 均未变）
        let row = sqlx::query!(
            r#"SELECT location AS "loc!", current_holder_id AS "ch?", version
            FROM t_part_batch WHERE id = $1"#,
            held_batch,
        )
        .fetch_one(&pool)
        .await
        .expect("query batch");
        assert_eq!(
            row.loc, "WORKER",
            "{name}: 拒收后 batch 应仍在 WORKER（location 未被写脏）"
        );
        assert_eq!(
            row.ch,
            Some(worker),
            "{name}: current_holder_id 必须仍指向 worker，未被写成目标货架"
        );
        assert_eq!(row.version, 0, "{name}: 拒收后 version 不应被自增");
        assert_eq!(
            count_held_by_worker(&pool, worker).await,
            1,
            "{name}: worker 仍持有该批次"
        );
    }
}

/// `current_process_id` 为 `None` 时选架**不按工序筛候选**，但 zone / 停用 / 软删
/// 三条谓词一条都不能松 —— 本用例造「一个 INSPECTION 架 + 一个 PRODUCTION 架」，
/// 断言批次落到 **PRODUCTION** 那个上。
///
/// 这是 WORKER→POOL 分支对「无工序归属批次」（`t_part_batch.current_process_id IS
/// NULL`：migration 004 之前的存量 + 历史脏数据）的刻意取舍：传 `None` 进选架的
/// `process_id` 形参即「不按工序筛」，管理员「把卡住的批次手动放回货架」的自救路径
/// 不被堵死；但 zone 谓词仍在候选集里，所以**不可能**把批次落到品检架上。
#[tokio::test]
async fn move_worker_to_pool_no_process_still_lands_on_production_zone_shelf() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "MPOOLNOPROC").await;
    let proc = seed_process(&pool, "PROC-MPNP", "工序MPNP").await;
    let wt = insert_work_type(&pool, "WT-MPNP", "工种MPNP", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-MPNP", "工MPNP", Some(wt)).await;
    // 品检架排在前面（display_order=0）：若 zone 谓词被漏掉，它会被选中
    let insp_shelf = insert_shelf_state(
        &pool,
        "SH-MPNP-INSP",
        "SH-MPNP-INSP",
        "INSPECTION",
        true,
        false,
    )
    .await;
    let prod_shelf = insert_shelf_state(
        &pool,
        "SH-MPNP-PROD",
        "SH-MPNP-PROD",
        "PRODUCTION",
        true,
        false,
    )
    .await;
    sqlx::query("UPDATE t_shelf SET display_order = 0 WHERE id = $1")
        .bind(insp_shelf)
        .execute(&pool)
        .await
        .expect("inspection shelf first");

    let (_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-MPNP", worker, proc, 1, true).await;
    // 把 current_process_id 清空 ⇒ service 侧 step_process_id = None
    sqlx::query("UPDATE t_part_batch SET current_process_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear current_process_id");

    let (app, token) = login_manager_with_username(&pool, "admin-mpnp").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                // 2026-10-09：move 的 OCC 锚必填；fixture 建批时 version 写死 0
                "version": 0,
                "from": { "kind": "WORKER", "worker_id": worker.to_string() },
                "to":   { "kind": "POOL" },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "无工序归属的批次仍可放回货架（自救路径不被堵死）: {env}"
    );
    assert_eq!(
        env["data"]["new_holder_id"],
        prod_shelf.to_string(),
        "必须落 PRODUCTION 架；品检架排在 display_order 前面仍被 zone 谓词挡掉: {env}"
    );

    let (loc, holder): (String, Option<i64>) =
        sqlx::query_as("SELECT location, current_holder_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("read batch");
    assert_eq!(loc, "PRODUCTION_SHELF");
    assert_eq!(holder, Some(prod_shelf));
}

// ===========================================================================
//  2026-10-09：`POST /prod/queue/move` 的 OCC 锚改为**客户端必填**
// ===========================================================================
//
// `MoveRequest.version` 新增且无 `#[serde(default)]`：三个方向一律以它作
// `expected_version`，0 行 → `40901 VERSION_CONFLICT`。
//
// 这三条用例钉住的是「客户端传值真的会挡住过期的看板快照」。先前三个方向用的
// 都是「本次事务里刚读到的 `batch.version`」或「SQL 内 `pb.version =
// candidate.version` 自比」，两者都恒真 —— 期间他人改过批次时，「用户看到 5 件 →
// 实际移动 3 件」会静默成功。三个方向各一条：三条 SQL 各自独立改过 WHERE，
// 少改一条就是这个用例红。

/// POOL→WORKER：过期 `version` → 40901，批次**不得**被切到 worker。
#[tokio::test]
async fn move_pool_to_worker_stale_version_returns_40901() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "OCC-PW").await;
    let proc = seed_process(&pool, "PROC-OCPW", "工序OCPW").await;
    let wt = insert_work_type(&pool, "WT-OCPW", "工种OCPW", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-OCPW", "PROD-OCPW", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;
    let worker = insert_worker(&pool, "BC-OCPW", "工OCPW", Some(wt)).await;
    let (_part, pool_batch) =
        insert_pool_part(&pool, customer, "P-OCPW", prod_shelf, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin-occpw").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/move",
            Some(json!({
                "batch_id": pool_batch.to_string(),
                // fixture 建批 version=0，这里传 +9 ⇒ 过期
                "version": 9,
                "from": { "kind": "POOL",   "shelf_id": prod_shelf.to_string() },
                "to":   { "kind": "WORKER", "worker_id": worker.to_string() },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "过期 version 应 409: {env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");

    // 批次不得被写脏：仍在货架上、holder 未变、version 未自增
    let row: (String, Option<i64>, i32) = sqlx::query_as(
        "SELECT location, current_holder_id, version FROM t_part_batch WHERE id = $1",
    )
    .bind(pool_batch)
    .fetch_one(&pool)
    .await
    .expect("query batch");
    assert_eq!(row.0, "PRODUCTION_SHELF", "拒收后 location 不应变");
    assert_eq!(row.1, Some(prod_shelf), "拒收后 holder 不应被改写成 worker");
    assert_eq!(row.2, 0, "拒收后 version 不应被自增");
    assert_eq!(count_held_by_worker(&pool, worker).await, 0);
}

/// WORKER→POOL：过期 `version` → 40901，批次**不得**回到货架。
#[tokio::test]
async fn move_worker_to_pool_stale_version_returns_40901() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "OCC-WP").await;
    let proc = seed_process(&pool, "PROC-OCWP", "工序OCWP").await;
    let wt = insert_work_type(&pool, "WT-OCWP", "工种OCWP", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-OCWP", "PROD-OCWP", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;
    let worker = insert_worker(&pool, "BC-OCWP", "工OCWP", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-OCWP", worker, proc, 1, true).await;

    let (app, token) = login_manager_with_username(&pool, "admin-occwp").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                "version": 9,
                "from": { "kind": "WORKER", "worker_id": worker.to_string() },
                "to":   { "kind": "POOL" },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "过期 version 应 409: {env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");

    let (loc, holder): (String, Option<i64>) =
        sqlx::query_as("SELECT location, current_holder_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("query batch");
    assert_eq!(loc, "WORKER", "拒收后批次应仍在 worker 手上");
    assert_eq!(holder, Some(worker), "拒收后 holder 不应被改成货架");
    assert_eq!(count_held_by_worker(&pool, worker).await, 1);
}

/// WORKER→WORKER：过期 `version` → 40901，批次**不得**易主。
#[tokio::test]
async fn move_worker_to_worker_stale_version_returns_40901() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "OCC-WW").await;
    let proc = seed_process(&pool, "PROC-OCWW", "工序OCWW").await;
    let wt = insert_work_type(&pool, "WT-OCWW", "工种OCWW", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-OCWW", "PROD-OCWW", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;
    let worker_src = insert_worker(&pool, "BC-OCWW1", "工OCWW1", Some(wt)).await;
    let worker_dst = insert_worker(&pool, "BC-OCWW2", "工OCWW2", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-OCWW", worker_src, proc, 1, true).await;

    let (app, token) = login_manager_with_username(&pool, "admin-occww").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/move",
            Some(json!({
                "batch_id": held_batch.to_string(),
                "version": 9,
                "from": { "kind": "WORKER", "worker_id": worker_src.to_string() },
                "to":   { "kind": "WORKER", "worker_id": worker_dst.to_string() },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "过期 version 应 409: {env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");

    assert_eq!(
        count_held_by_worker(&pool, worker_src).await,
        1,
        "源工人仍持有"
    );
    assert_eq!(
        count_held_by_worker(&pool, worker_dst).await,
        0,
        "目标工人未拿到"
    );
}

/// 2026-10-09：`has_process_chain` 派生列（卡片绿色左边框的判据）。
///
/// 覆盖 4 处落点里的 3 个端点（外协候选卡在 `tests/outsource/pool.rs`，因为那边
/// 才有 OUTSOURCE 类工序与候选 fixture）。四种批次形态 × 三个端点：
///
/// | 形态 | `has_process_chain` | 走的判据分支 |
/// |---|---|---|
/// | 已绑链 + 指针指向当前工序所在 step | `true` | 分支 1 |
/// | 已绑链 + **未定位**（`current_process_id` 与指针都 NULL）+ 链内有活跃 step | `true` | 分支 2 的 EXISTS 成立 |
/// | 无链 | `false` | 首个条件 `process_chain_id IS NOT NULL` 即否 |
/// | 已绑链 + **未定位** + **链内零活跃 step** | `false` | 两条分支都不成立（**算子选型的唯一可观测形态**，见下） |
///
/// ⚠️ 「未定位」这一形态用 `IN_PROCESS` + `WORKER` 的批次而不是字面的 `PENDING`
/// 批次：4 个列表的谓词都硬限定 `status='IN_PROCESS'`（候选池还额外要求
/// `current_process_id = $1`），`PENDING` 批次根本不出现在这 4 个端点里。判据要
/// 验的是「工序未定位但工单有链」，用同形态的行才打得到那条 EXISTS 分支。
///
/// ⚠️ 判据必须用 `IS NOT NULL AND =` 而不是 `IS NOT DISTINCT FROM`，但**前三种形态
/// 钉不住这个区别**（钉的是取值、不是算子选型）：C 那条两种写法都是 `true`（分支 1
/// 显式否掉后落到分支 2 的 EXISTS），B/D 那条两种写法都在首个条件就否掉。
/// **能区分两种写法的只有 E**（已绑链 + 未定位 + 链内零活跃 step）：此时 `cs` 无行
/// （指针 NULL）故 `cs.process_id` 与 `pb.current_process_id` 同为 NULL ——
///   - 现写法：分支 1 因 `cs.process_id IS NOT NULL` 为假而否掉，分支 2 因 EXISTS
///     找不到活跃 step 而否掉 ⇒ **`false`**；
///   - `IS NOT DISTINCT FROM` 写法：分支 1 的 `NULL IS NOT DISTINCT FROM NULL` 为
///     真 ⇒ 误判成 `true`（把「有链但链内一道都没了」的批次也画上绿框）。
#[tokio::test]
async fn has_process_chain_reflects_chain_and_pointer_state() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL-HPC").await;
    let proc = seed_process(&pool, "PROC-HPC", "工序-HPC").await;
    let wt = insert_work_type(&pool, "WT-HPC", "工种-HPC", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-HPC", "PROD-HPC", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;
    let worker = insert_worker(&pool, "BC-HPC", "工HPC", Some(wt)).await;

    // A. 已绑链 + 指针一致（insert_pool_part 建链、绑 part，并把批次指针指向该
    //    step，而 step.process_id == 批次 current_process_id）
    let (part_a, _batch_a) = insert_pool_part(&pool, customer, "HPC-A", prod_shelf, proc, 1).await;
    // B. 无链（同一 helper 建完后把链摘掉 ⇒ process_chain_id IS NULL）
    let (part_b, _batch_b) = insert_pool_part(&pool, customer, "HPC-B", prod_shelf, proc, 1).await;
    sqlx::query("UPDATE t_part SET process_chain_id = NULL WHERE id = $1")
        .bind(part_b)
        .execute(&pool)
        .await
        .expect("unbind part B from chain");
    // C. 已绑链 + 未定位（指针与工序都清 NULL ⇒ 走分支 2）
    let (part_c, batch_c, _step_c) =
        insert_worker_held_part(&pool, customer, "HPC-C", worker, proc, 1, true).await;
    sqlx::query(
        "UPDATE t_part_batch SET current_process_id = NULL, current_process_step_id = NULL \
         WHERE id = $1",
    )
    .bind(batch_c)
    .execute(&pool)
    .await
    .expect("clear batch C position");
    // D. 无链 + 未定位
    let (part_d, batch_d, _step_d) =
        insert_worker_held_part(&pool, customer, "HPC-D", worker, proc, 1, false).await;
    sqlx::query(
        "UPDATE t_part_batch SET current_process_id = NULL, current_process_step_id = NULL \
         WHERE id = $1",
    )
    .bind(batch_d)
    .execute(&pool)
    .await
    .expect("clear batch D position");
    // E. 已绑链 + 未定位 + **链内零活跃 step**（唯一能区分 `IS NOT NULL AND =` 与
    //    `IS NOT DISTINCT FROM` 的形态，理由见本用例 doc）
    let (part_e, batch_e, _step_e) =
        insert_worker_held_part(&pool, customer, "HPC-E", worker, proc, 1, true).await;
    sqlx::query(
        "UPDATE t_part_batch SET current_process_id = NULL, current_process_step_id = NULL \
         WHERE id = $1",
    )
    .bind(batch_e)
    .execute(&pool)
    .await
    .expect("clear batch E position");
    sqlx::query(
        "UPDATE t_process_chain_step SET deleted_at = now() \
                WHERE chain_id = (SELECT process_chain_id FROM t_part WHERE id = $1) \
                  AND deleted_at IS NULL",
    )
    .bind(part_e)
    .execute(&pool)
    .await
    .expect("soft-delete all steps of chain E");
    // 前置断言：五个 part 的链绑定与批次位置必须与用例名一致，否则后面的断言
    // 会在「数据没造对」的前提下假绿。
    let chain_of = async |part_id: i64| {
        sqlx::query_scalar::<_, Option<i64>>("SELECT process_chain_id FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("read process_chain_id")
    };
    assert!(chain_of(part_a).await.is_some(), "A 应已绑链");
    assert!(chain_of(part_b).await.is_none(), "B 应无链");
    assert!(chain_of(part_c).await.is_some(), "C 应已绑链");
    assert!(chain_of(part_d).await.is_none(), "D 应无链");
    assert!(chain_of(part_e).await.is_some(), "E 应已绑链");
    // E 的「链内零活跃 step」前置：EXISTS 必须落空，否则 E 就退化成 C、钉不住算子选型
    let e_active_steps: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM t_process_chain_step \
         WHERE chain_id = $1 AND deleted_at IS NULL",
    )
    .bind(chain_of(part_e).await.expect("E 的 chain_id"))
    .fetch_one(&pool)
    .await
    .expect("count active steps of chain E");
    assert_eq!(e_active_steps, 0, "E 的链内必须一个活跃 step 都没有");

    let (app, token) = login_manager_with_username(&pool, "admin_hpc").await;

    // ---- 端点 a/b：GET /prod/queue/processes/{proc} ----
    let uri = format!("/prod/queue/processes/{proc}");
    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "board process detail: {env}");
    let item_of = |part_id: i64| -> Value {
        env["data"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|i| i["part_id"] == json!(part_id.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("候选池应含 part {part_id}: {env}"))
    };
    assert_eq!(
        item_of(part_a)["has_process_chain"],
        json!(true),
        "A（已绑链 + 指针一致）⇒ true: {env}"
    );
    assert_eq!(
        item_of(part_b)["has_process_chain"],
        json!(false),
        "B（无链）⇒ false: {env}"
    );
    assert!(
        item_of(part_a)["has_process_chain"].is_boolean(),
        "出参必须是 JSON boolean（不是 0/1、不是字符串）: {env}"
    );

    let held_of = |part_id: i64| -> Value {
        env["data"]["workers"]
            .as_array()
            .expect("workers")
            .iter()
            .flat_map(|w| {
                w["held_batches"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
            })
            .find(|h| h["part_id"] == json!(part_id.to_string()))
            .unwrap_or_else(|| panic!("held_batches 应含 part {part_id}: {env}"))
    };
    assert_eq!(
        held_of(part_c)["has_process_chain"],
        json!(true),
        "C（已绑链 + 未定位）⇒ 走 EXISTS 分支 ⇒ true: {env}"
    );
    assert_eq!(
        held_of(part_d)["has_process_chain"],
        json!(false),
        "D（无链）⇒ false: {env}"
    );
    assert_eq!(
        held_of(part_e)["has_process_chain"],
        json!(false),
        "E（已绑链 + 未定位 + 链内零活跃 step）⇒ 两条分支都不成立 ⇒ false: {env}"
    );

    // ---- 端点 d（之一）：GET /parts/pickable-by-work-type/{wt_id} ----
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/parts/pickable-by-work-type/{wt}"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "pickable-by-work-type: {env2}");
    let pickable_of = |part_id: i64| -> Value {
        env2["data"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|i| i["id"] == json!(part_id.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("可领列表应含 part {part_id}: {env2}"))
    };
    assert_eq!(
        pickable_of(part_a)["has_process_chain"],
        json!(true),
        "pickable：A（已绑链 + 指针一致）⇒ true: {env2}"
    );
    assert_eq!(
        pickable_of(part_b)["has_process_chain"],
        json!(false),
        "pickable：B（无链）⇒ false: {env2}"
    );

    // ---- 端点 d（之二）：GET /parts/by-worker/{worker_id} ----
    let (s3, env3) = send(
        app,
        json_request(
            "GET",
            &format!("/parts/by-worker/{worker}"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::OK, "by-worker: {env3}");
    let held_item_of = |part_id: i64| -> Value {
        env3["data"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|i| i["id"] == json!(part_id.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("持有列表应含 part {part_id}: {env3}"))
    };
    assert_eq!(
        held_item_of(part_c)["has_process_chain"],
        json!(true),
        "by-worker：C（已绑链 + 未定位）⇒ true: {env3}"
    );
    assert_eq!(
        held_item_of(part_d)["has_process_chain"],
        json!(false),
        "by-worker：D（无链）⇒ false: {env3}"
    );
    assert_eq!(
        held_item_of(part_e)["has_process_chain"],
        json!(false),
        "by-worker：E（已绑链 + 未定位 + 链内零活跃 step）⇒ false: {env3}"
    );
    // 链四字段不受本次改动影响（那是给放回页分流用的）
    assert_eq!(
        held_item_of(part_c)["chain_state"],
        json!("NONE"),
        "C 的指针与工序都为空 ⇒ chain_state 仍是 NONE（本次不动它）: {env3}"
    );
}

// ===========================================================================
//  2026-10-10：worker-scan 自动选架 / 链尾自动送检 / refill 跨架取料
// ===========================================================================

/// RETURNED：目标架由服务端按负载选出，请求里**没有** `shelf_id`。
///
/// 两个映射架按 `display_order` 排成 A / B，但 `capacity` + 在架件数排成另一条次序
/// （A 80%、B 20%），断言落 B —— 于是「按物理顺序取第一个」与「按负载取最空」两种
/// 口径被分开。批次 `current_holder_id` 就是被断言的那个架 id。
#[tokio::test]
async fn worker_scan_returned_picks_least_loaded_shelf() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "AUTO-PICK").await;
    let proc_a = seed_process(&pool, "AP-P1", "AP1").await;
    let proc_b = seed_process(&pool, "AP-P2", "AP2").await;
    let wt = insert_work_type(&pool, "AP-WT", "AP工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_a).await;

    let shelf_a = insert_shelf(&pool, "AP-SH-A", "AP架A", "PRODUCTION").await;
    let shelf_b = insert_shelf(&pool, "AP-SH-B", "AP架B", "PRODUCTION").await;
    for shelf in [shelf_a, shelf_b] {
        sqlx::query("UPDATE t_shelf SET capacity = 100, display_order = $2 WHERE id = $1")
            .bind(shelf)
            .bind(if shelf == shelf_a { 0_i32 } else { 1 })
            .execute(&pool)
            .await
            .expect("set capacity/display_order");
        link_shelf_to_process(&pool, shelf, proc_b).await;
    }
    // 在架负载：A 80 件、B 20 件（`SUM(quantity)` 件数口径）
    let (_pa, _ba) = insert_pool_part(&pool, customer, "AP-LOAD-A", shelf_a, proc_b, 80).await;
    let (_pb, _bb) = insert_pool_part(&pool, customer, "AP-LOAD-B", shelf_b, proc_b, 20).await;

    let worker = insert_worker(&pool, "AP-W1", "AP工人", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "AP-HELD", worker, proc_a, 1, false).await;
    // 清 step 指针压到「非顺应 ⇒ 显式 `next_process_id`」分支：fixture 的链是单
    // step 链，指针一致时会被判成 `TAIL` 并被「链尾自动送检」接管（那条路径由
    // `worker_scan_returned_at_chain_tail_auto_sends_to_inspection` 覆盖）
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "ap-user", &[shelf_a, shelf_b]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "AP-HELD",
                "badge_code": "AP-W1",
                "event_type": "RETURNED",
                "next_process_id": proc_b.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_RETURNED");

    let (holder, location, current_process): (Option<i64>, Option<String>, Option<i64>) =
        sqlx::query_as(
            "SELECT current_holder_id, location, current_process_id FROM t_part_batch WHERE id = $1",
        )
        .bind(held_batch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        holder,
        Some(shelf_b),
        "应落负载比例最低的架（20%），不是 display_order 最小的 A（80%）"
    );
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(current_process, Some(proc_b), "工序应推进到目标工序");
}

/// 链尾自动送检：单 step 链 + 指针一致 ⇒ RETURNED 直接送检。
///
/// 断言四件事：HTTP 200、响应 `event_type = "WORKER_SCAN_INSPECTED"`（**与请求的
/// `RETURNED` 不同** —— 这是前端必须按响应分支的那条语义）、批次
/// `status = INSPECTION` + `location = INSPECTION_SHELF`、holder 是服务端选出的
/// 品检架。
#[tokio::test]
async fn worker_scan_returned_at_chain_tail_auto_sends_to_inspection() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TAIL").await;
    let proc_only = seed_process(&pool, "TAIL-P", "链尾唯一工序").await;
    let wt = insert_work_type(&pool, "TAIL-WT", "TAIL工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_only).await;

    let prod_shelf = insert_shelf(&pool, "TAIL-SH", "TAIL架", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_only).await;
    let insp_shelf = insert_shelf(&pool, "TAIL-INSP", "TAIL品检架", "INSPECTION").await;

    // `with_chain = true` ⇒ part 绑链，链内**只有一道** step（= 链尾），且批次指针
    // 指向它 ⇒ `is_pointer_consistent = true` ∧ `chain_state == "TAIL"`
    let worker = insert_worker(&pool, "TAIL-W1", "TAIL工人", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "TAIL-HELD", worker, proc_only, 1, true).await;

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "tail-user", &[prod_shelf, insp_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            // 刻意**不传** `next_process_id`：链尾没有下一道，它本来就该可省
            Some(json!({
                "serial_no": "TAIL-HELD",
                "badge_code": "TAIL-W1",
                "event_type": "RETURNED",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "链尾 RETURNED 应自动送检: {env}");
    assert_eq!(
        env["data"]["scan"]["event_type"], "WORKER_SCAN_INSPECTED",
        "响应的 event_type 必须反映实际发生的动作（链尾 ⇒ 送检）: {env}"
    );

    let (status, location, holder): (String, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT status, location, current_holder_id FROM t_part_batch WHERE id = $1",
    )
    .bind(held_batch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "INSPECTION");
    assert_eq!(location.as_deref(), Some("INSPECTION_SHELF"));
    assert_eq!(holder, Some(insp_shelf), "应落服务端自动选出的品检架");
}

/// refill 跨架取料：worker-scan 路径传 `shelf_id = None`，候选池**不限架**。
///
/// 两个生产架各有一个在架批次；放回时落 A 架（只给 A 架配了映射），随后的 refill 若
/// 还按架取料就只能拿到 A 架那一批 —— 断言它拿到了**两个架**的批次即证明跨架生效。
#[tokio::test]
async fn refill_takes_across_all_shelves_without_shelf_anchor() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "XSHELF").await;
    let proc_a = seed_process(&pool, "XS-P1", "XS1").await;
    let proc_b = seed_process(&pool, "XS-P2", "XS2").await;
    let wt = insert_work_type(&pool, "XS-WT", "XS工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc_a).await;
    link_work_type_to_process(&pool, wt, proc_b).await;

    // A 架只映射 proc_a（RETURNED 的目标架）；B 架映射 proc_b，且预置一个批次
    let shelf_a = insert_shelf(&pool, "XS-SH-A", "XS架A", "PRODUCTION").await;
    let shelf_b = insert_shelf(&pool, "XS-SH-B", "XS架B", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_a, proc_a).await;
    link_shelf_to_process(&pool, shelf_b, proc_b).await;
    let (_pb, batch_b) = insert_pool_part(&pool, customer, "XS-POOL-B", shelf_b, proc_b, 1).await;

    let worker = insert_worker(&pool, "XS-W1", "XS工人", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "XS-HELD", worker, proc_a, 1, false).await;
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");

    let (app, token, _pool) = login_shelf_account(pool.clone(), "xs-user", &[shelf_a]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/worker-scan",
            Some(json!({
                "serial_no": "XS-HELD",
                "badge_code": "XS-W1",
                "event_type": "RETURNED",
                "next_process_id": proc_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    let taken = env["data"]["refill"]["taken"]
        .as_array()
        .expect("refill.taken");
    let batch_ids: Vec<String> = taken
        .iter()
        .map(|t| t["batch_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        taken.len(),
        2,
        "refill 应跨两个架各取一件（放回那件 + B 架原有那件）: {env}"
    );
    assert!(
        batch_ids.contains(&batch_b.to_string()),
        "必须取到 B 架的批次 {batch_b}（证明不限架）: {env}"
    );
}
