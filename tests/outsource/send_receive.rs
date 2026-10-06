//! outsource shipment 集成测试（Phase 2 2026-09-13）
//!
//! 覆盖（基于 prod 域批次的 shipment 写入 + reconcile 端点）：
//! - send-to-outsource INSERT shipment + quote event SENT
//! - receive-from-outsource 标 shipment RECEIVED + quote event RECEIVED
//! - reconcile-update OCC 守
//! - DIRECT 免审批直发（2026-10-03：复用活跃报价 / 自动建 0 元占位报价 / 与
//!   quote_id 互斥 / 必须给价来源）
//! - 部分发送 / 部分接收（2026-10-03：拆批语义 + shipment 记账口径）
//! - 拆批的 OCC 契约（2026-10-03：源批次 version +1、子批次用读回行 version 作锚、
//!   二次收发必须先刷新列表）
//! - 部分收发的派生契约（min-progress：部分发送后 part 停在 `PENDING`、部分接收
//!   后 part 变 `IN_PROCESS`）
//! - DIRECT 占位报价唯一性（migration 008：同 tuple 只留 1 条 `is_direct=true`）
//! - **需审批工序不许直发**（2026-10-03 review 第 1 轮：`requires_approval=true` +
//!   `direct=true` → 400/20104，且不建占位报价、不开 shipment；同工序走 APPROVAL
//!   仍放行）
//! - **占位报价不能当审批价**（2026-10-03 review 第 2 轮：`quote_id` 指向
//!   `is_direct=true` 的 APPROVED 占位报价 → 400/21307，不开 shipment；免审批工序
//!   走 DIRECT 复用占位报价仍放行）
//! - 补齐后的守卫（process 类别必须 OUTSOURCE / 公司必须映射该工序）
//! - **无工艺链零件的收发闭环**（2026-10-03：part 没有 `process_chain_id` 时
//!   `send-to-outsource` / `receive-from-outsource` 仍返 200，
//!   `current_process_step_id` 落 NULL）
//! - **20702 不被「链可选」一起吞掉**（2026-10-03：part 有链但链内没有该外协工序的
//!   step 时，`send-to-outsource` 仍以 20702 / HTTP 404 拒收）
//! - **IN_PROCESS 源能发**（2026-10-03：端到端实测发现的阻塞 bug —— 状态机白名单缺
//!   `IN_PROCESS → OUTSOURCE`，「可发送一览」的行发一单就被 20103 拒。补边后
//!   `IN_PROCESS + PRODUCTION_SHELF` 的无链零件 `direct=true` 发送 200；配对的负向
//!   用例证明 `location != PRODUCTION_SHELF` 仍被 service 守卫拒在 20103）
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
//!   软删级联 + 让收发路径能解析到链内 step）；
//! - 域独享 helper 不从 `fixtures` 模块 `use`（Phase H gate 5 禁止）；
//!   本地 helper 用 `sqlx::query` 直插与 `fixtures::*` 同形 SQL。
//!
//! ## 不预置 t_part / t_part_batch / t_part_process_chain / t_process_chain_step /
//!  t_outsource_quote / t_outsource_shipment
//! 状态机不允许 part 从 OUTSOURCE 回退 PENDING；每个测试要按需造不同
//! (part, batch, company, process, quote) 组合 + 自建 chain/step。预置会污染
//! list / count 等「期望空库」断言。各 sub-file 用本地 helper 直插。
//!
//! ## 2026-10-03：发外协用例必须先映射 company↔process
//! `send-to-outsource` 新增「公司必须映射该外协工序」守卫（`t_outsource_
//! company_process` 存在未软删行），所以每个发外协用例都要调
//! `map_company_process()` —— 守卫本身由
//! `send_to_outsource_rejects_company_without_process_mapping` 单独锁住。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, send, test_app, test_pool,
    test_state,
};

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

/// 直插批次（任意 status + 可选 location），`quantity` 固定 5。
async fn insert_batch(pool: &PgPool, part_id: i64, status: &str, location: Option<&str>) -> i64 {
    insert_batch_with_qty(pool, part_id, status, location, 5).await
}

/// 同 [`insert_batch`]，但数量可指定（部分收发的断言需要知道源批次余量）。
async fn insert_batch_with_qty(
    pool: &PgPool,
    part_id: i64,
    status: &str,
    location: Option<&str>,
    qty: i32,
) -> i64 {
    insert_nth_batch(pool, part_id, 1, status, location, qty).await
}

/// 直插同 part 下的**第 n 个**批次。
///
/// 与 [`insert_batch_with_qty`] 分开是因为 `uq_t_part_batch_part_no` 要求同
/// `part_id` 下 `batch_no` 互异，造第二个批次时不能再写死 1。
async fn insert_nth_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    status: &str,
    location: Option<&str>,
    qty: i32,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch \
         (id, part_id, batch_no, quantity, status, location, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, 0, $7, $7)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(qty)
    .bind(status)
    .bind(location)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 2026-10-03 新增：直插「**已入池**」的批次 —— `current_process_id` 指向一道工序。
///
/// 「可发送一览」的行按该列出行（而不是按状态），所以 IN_PROCESS 源的发送用例
/// 必须造出这个组合，否则验的不是真实数据形态。`current_holder_id` 留 NULL
/// （生产架批次本来就记货架 id，本用例不验它）。
async fn insert_batch_with_process(
    pool: &PgPool,
    part_id: i64,
    status: &str,
    location: Option<&str>,
    current_process_id: i64,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch \
         (id, part_id, batch_no, quantity, status, location, current_process_id, version, \
          created_at, updated_at) \
         VALUES ($1, $2, 1, 5, $3, $4, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(location)
    .bind(current_process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch with current_process_id");
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

/// 直插任意 category 的 process。
///
/// 2026-10-03 review 第 1 轮：`requires_approval` 变成形参。原先本 helper 一律写
/// `true`，在「该列只写不读」时无害；现在它决定两件事 —— 写侧 `send-to-outsource`
/// 拒 `requires_approval=true` + `direct=true`（20104），读侧 sendable / pool 判定
/// 该 (part, process) 需不需要先有审批报价。DIRECT 用例必须显式传 `false`，
/// 否则它会挂在「直发被拒」而不是它自己声称验证的那条路径上。
async fn seed_process(
    pool: &PgPool,
    code: &str,
    name: &str,
    category: &str,
    requires_approval: bool,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 0, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(category)
    .bind(requires_approval)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

/// 直插 OUTSOURCE 类别 process。
async fn seed_outsource_process(
    pool: &PgPool,
    code: &str,
    name: &str,
    requires_approval: bool,
) -> i64 {
    seed_process(pool, code, name, "OUTSOURCE", requires_approval).await
}

/// 2026-10-03 新增：直插 `t_outsource_company_process`（公司 ↔ 工序映射）。
///
/// `send-to-outsource` 新增守卫：该公司必须映射该外协工序，否则 400。
async fn map_company_process(pool: &PgPool, company_id: i64, process_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company_process \
         (id, outsource_company_id, process_id, sort_order, version, created_at, created_by, \
          updated_at, updated_by) \
         VALUES ($1, $2, $3, 0, 0, $4, 0, $4, 0)",
    )
    .bind(id)
    .bind(company_id)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company_process");
    id
}

/// 直插 APPROVED 状态 quote（绕开 DRAFT→SUBMITTED→APPROVED 状态机）。
async fn insert_approved_quote(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
) -> i64 {
    insert_quote_with_price(pool, part_id, company_id, process_id, "12.50").await
}

/// 同 [`insert_approved_quote`]，单价可指定（DIRECT 复用要断言单价透传）。
async fn insert_quote_with_price(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
    price: &str,
) -> i64 {
    insert_quote_with_flags(pool, part_id, company_id, process_id, price, false).await
}

/// 2026-10-03 review 第 2 轮：直插 `is_direct = true` 的 APPROVED 报价，即
/// `resolve_direct_quote_id` 自动建的那种「免审批直发占位价」（`price` 通常 0）。
///
/// 只能直插而不能靠调端点造出来：需审批工序上的 DIRECT 已被写侧守卫拒（见
/// `send_to_outsource_direct_rejected_when_process_requires_approval`），而
/// 真实数据里这批行来自守卫上线之前的历史数据、或 `requires_approval` 由 false
/// 翻成 true 的存量工序。
async fn insert_direct_placeholder_quote(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
) -> i64 {
    insert_quote_with_flags(pool, part_id, company_id, process_id, "0", true).await
}

async fn insert_quote_with_flags(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
    price: &str,
    is_direct: bool,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_quote \
         (id, part_id, outsource_company_id, process_id, price, status, submitted_at, \
          reviewed_at, review_note, is_direct, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5::numeric, 'APPROVED', $6, \
                 $6, 'OK', $7, 0, $6, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(company_id)
    .bind(process_id)
    .bind(price)
    .bind(now)
    .bind(is_direct)
    .execute(pool)
    .await
    .expect("insert t_outsource_quote");
    id
}

/// 批次 step 化（migration 028）：收发两端点把 `current_process_step_id` 写成
/// 「part 的锚链内、该 process 的活跃 step」；2026-10-03 起链本身**不是必需**
/// （`optional_process_chain`，无链放行且 step 落 NULL），但一旦有链，链内缺该
/// 工序的 step 会被 `optional_step_id` 以 20702 拒收。本 helper 帮 part 建链 + 绑 part。
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

/// 直插一张货架（`zone` = `PRODUCTION` / `INSPECTION`）。
async fn insert_shelf(pool: &PgPool, code: &str, zone: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 'Recv', $3, true, 0, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(zone)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

/// 直插 `t_shelf_process`（货架 ↔ 工序映射）。
async fn map_shelf_process(pool: &PgPool, shelf_id: i64, process_id: i64) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
         created_at, updated_at) VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(id)
    .bind(shelf_id)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
}

/// 外协回收侧（回生产架）的成套 fixture：PRODUCTION 货架 + 映射「下一道工序」+
/// 在 part 的 chain 里为该工序建 step。
///
/// `resolve_step_id_by_process` 守的就是那条 step：缺了端点会以
/// `BIZ_PROCESS_CHAIN_STEP_NOT_FOUND` 422 拒绝，故 create_step 不能省。
///
/// 返回 `(shelf_id, next_process_id)`。
async fn setup_receive_side(
    pool: &PgPool,
    chain_id: i64,
    shelf_code: &str,
    next_proc_code: &str,
) -> (i64, i64) {
    let shelf_id = insert_shelf(pool, shelf_code, "PRODUCTION").await;
    let next_proc = seed_outsource_process(pool, next_proc_code, "recv_proc", true).await;
    map_shelf_process(pool, shelf_id, next_proc).await;
    create_step(pool, chain_id, next_proc, 2).await;
    (shelf_id, next_proc)
}

/// 外协在途态的成套 fixture：OUTSOURCE 批次（`location='OUTSOURCE_COMPANY'`）+
/// 开口 `OUTSOURCING` shipment（数量 = 发出时的全量 = `batch_qty`）+ 回收目标架。
///
/// 返回 `(batch_id, quote_id, shelf_id, next_process_id)`。
#[allow(clippy::too_many_arguments)]
async fn setup_inflight(
    pool: &PgPool,
    name: &str,
    prefix: &str,
    proc_code: &str,
    shelf_code: &str,
    next_proc_code: &str,
    batch_qty: i32,
) -> (i64, i64, i64, i64) {
    let customer_id = insert_l1_customer(pool, name, prefix).await;
    let part_id = insert_part(pool, customer_id, "PENDING").await;
    let bid = insert_batch_with_qty(
        pool,
        part_id,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        batch_qty,
    )
    .await;
    let company_id = insert_outsource_company(pool, &format!("{name}Co")).await;
    let proc_id = seed_outsource_process(pool, proc_code, "proc", true).await;
    let quote_id = insert_approved_quote(pool, part_id, company_id, proc_id).await;
    let chain_id = create_chain_for_part(pool, part_id).await;
    create_step(pool, chain_id, proc_id, 1).await;
    let now = now_naive();
    let shipment_id: i64 = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_shipment \
         (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
          quantity, unit_price, status, sent_at, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 12.50, 'OUTSOURCING', $8, 0, $8, $8)",
    )
    .bind(shipment_id)
    .bind(quote_id)
    .bind(part_id)
    .bind(bid)
    .bind(company_id)
    .bind(proc_id)
    .bind(batch_qty)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_shipment");
    let (shelf_id, next_proc) =
        setup_receive_side(pool, chain_id, shelf_code, next_proc_code).await;
    (bid, quote_id, shelf_id, next_proc)
}

/// 查某 part 名下全部活跃批次的 `(id, quantity, status, location, version)`。
async fn list_batches(pool: &PgPool, part_id: i64) -> Vec<(i64, i32, String, Option<String>, i32)> {
    sqlx::query_as(
        "SELECT id, quantity, status, location, version FROM t_part_batch \
         WHERE part_id = $1 AND deleted_at IS NULL ORDER BY batch_no",
    )
    .bind(part_id)
    .fetch_all(pool)
    .await
    .expect("list t_part_batch")
}

/// 读 `t_part` 的派生 `status`（批次 min-progress 派生的缓存列）。
async fn part_status(pool: &PgPool, part_id: i64) -> String {
    sqlx::query_scalar("SELECT status FROM t_part WHERE id = $1")
        .bind(part_id)
        .fetch_one(pool)
        .await
        .expect("read t_part.status")
}

/// 2026-10-03 新增：part **完全没有** `process_chain_id` 时，send → receive 全链路
/// 仍然走通，`current_process_step_id` 落 NULL。
///
/// 无链零件发外协曾被 `20706 BIZ_PROCESS_CHAIN_REQUIRED`「请先制定工序链」拦在 send
/// 之前，而生产库里 1874 个零件只有 2 个绑了链 ⇒ 绝大多数货根本发不出去（外协
/// 「可发送」列表恒空也是同一个根因）。`current_process_step_id` 早已被官方降级为
/// 「可选的显示用定位信息」，写 NULL 有 `dispatch` 路径的先例。
#[tokio::test]
async fn send_and_receive_without_process_chain_succeeds() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "NoChain", "N").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "NoChainCo").await;
    let proc_id = seed_outsource_process(&pool, "PNC-SND", "noc_send", true).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    map_company_process(&pool, company_id, proc_id).await;
    // 前提断言：part 确实没有链
    let chain: Option<i64> =
        sqlx::query_scalar("SELECT process_chain_id FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("read process_chain_id");
    assert!(chain.is_none(), "本用例前提是 part 无工艺链");

    // ---- send ----
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "无链零件也必须能发外协: {env}");
    assert_eq!(env["data"]["status"], "OUTSOURCE");
    let (status, location, holder, cur_proc, step): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, location, current_holder_id, current_process_id, \
                    current_process_step_id \
             FROM t_part_batch WHERE id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("read batch after send");
    assert_eq!(status, "OUTSOURCE");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(holder, Some(company_id));
    assert_eq!(
        cur_proc,
        Some(proc_id),
        "池归属锚 current_process_id 必须写"
    );
    assert!(
        step.is_none(),
        "无链时 current_process_step_id 必须落 NULL（该列是可选的显示用定位信息）"
    );

    // ---- receive ----
    let shelf_id = insert_shelf(&pool, "NOC-REC", "PRODUCTION").await;
    let next_proc = seed_outsource_process(&pool, "PNC-REC", "noc_recv", true).await;
    map_shelf_process(&pool, shelf_id, next_proc).await;
    let version: i32 = sqlx::query_scalar("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .expect("read batch version");
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource"),
            Some(json!({
                "version": version,
                "shelf_id": shelf_id.to_string(),
                "next_process_id": next_proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "无链零件也必须能收回: {env}");
    assert_eq!(env["data"]["status"], "IN_PROCESS");
    let (status, location, cur_proc, step): (String, Option<String>, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT status, location, current_process_id, current_process_step_id \
             FROM t_part_batch WHERE id = $1",
        )
        .bind(bid)
        .fetch_one(&pool)
        .await
        .expect("read batch after receive");
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(cur_proc, Some(next_proc), "回池后归属下一道工序");
    assert!(step.is_none(), "无链时回收后 step 仍为 NULL");
}

/// 2026-10-03 新增：`IN_PROCESS` 源（`location='PRODUCTION_SHELF'`）的无链零件发送
/// **成功** —— 端到端实测曾发现这里 100% 被拒。
///
/// 根因是状态机白名单只有 `PENDING → OUTSOURCE`（`part/statemachine.rs`），没有
/// `IN_PROCESS → OUTSOURCE`，而 `send_to_outsource` 拿 batch 的源状态去
/// `ensure_transition(.., OUTSOURCE, ..)` ⇒ 一律 20103。影响面是全量：
/// 「可发送一览」按 `current_process_id` 出行，而按写入不变式
/// `PENDING ⇔ 出池（current_process_id 置 NULL）`，出行的批次几乎全是
/// `IN_PROCESS` 源 ⇒ 整个外协发送功能不可用。
///
/// 造的组合与真实数据形态一致：part **无** `process_chain_id`、批次
/// `IN_PROCESS + PRODUCTION_SHELF + current_process_id → OUTSOURCE 工序`、
/// 工序 `requires_approval=false`（免审批直发）。
#[tokio::test]
async fn send_to_outsource_from_in_process_shelf_batch_succeeds() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "InProcSnd", "Z").await;
    let part_id = insert_part(&pool, customer_id, "IN_PROCESS").await;
    let company_id = insert_outsource_company(&pool, "InProcSndCo").await;
    let proc_id = seed_outsource_process(&pool, "ZIPS", "inproc_send", false).await;
    map_company_process(&pool, company_id, proc_id).await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
    )
    .await;
    // 前提断言：part 确实没有链（无链时 step 落 NULL，是本用例的一半考点）
    let chain: Option<i64> =
        sqlx::query_scalar("SELECT process_chain_id FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("read process_chain_id");
    assert!(chain.is_none(), "本用例前提是 part 无工艺链");

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "direct": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "IN_PROCESS 源在生产架上必须能发外协: {env}"
    );
    assert_eq!(env["data"]["status"], "OUTSOURCE", "{env}");

    let (status, location, holder, cur_proc, step): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, location, current_holder_id, current_process_id, \
                current_process_step_id \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("read batch after send");
    assert_eq!(status, "OUTSOURCE");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(holder, Some(company_id), "holder 写外协公司");
    assert_eq!(
        cur_proc,
        Some(proc_id),
        "批次归属锚 current_process_id 写该 OUTSOURCE 工序（收回时按它重新入池）"
    );
    assert!(
        step.is_none(),
        "无链时 current_process_step_id 必须落 NULL（该列是可选的显示用定位信息）"
    );

    // shipment 照常开一张开口单；part 由 min-progress 派生为 OUTSOURCE
    let ship: (String, i64, i64) = sqlx::query_as(
        "SELECT status, batch_id, process_id FROM t_outsource_shipment WHERE batch_id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("shipment row");
    assert_eq!(ship.0, "OUTSOURCING");
    assert_eq!(ship.1, bid);
    assert_eq!(ship.2, proc_id);
    assert_eq!(part_status(&pool, part_id).await, "OUTSOURCE");
}

/// 2026-10-03 新增：`IN_PROCESS` 源但 `location != 'PRODUCTION_SHELF'`（例如在工人
/// 手上）仍**拒收**，20103。
///
/// 这条与上一条成对：状态机补 `IN_PROCESS → OUTSOURCE` 后，`IN_PROCESS` 源的
/// 不变式就只剩 `send_to_outsource` 里那道 location 守卫（与 recall-to-pending /
/// to-inspection 同一分工）。本用例证明守卫没被一起放松 —— 货还在工人手上时
/// 不该被发到外协。
#[tokio::test]
async fn send_to_outsource_rejects_in_process_off_production_shelf() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "InProcOff", "Y").await;
    let part_id = insert_part(&pool, customer_id, "IN_PROCESS").await;
    let company_id = insert_outsource_company(&pool, "InProcOffCo").await;
    let proc_id = seed_outsource_process(&pool, "YIPO", "inproc_off", false).await;
    map_company_process(&pool, company_id, proc_id).await;
    // location = WORKER：货在工人手上，不在生产架
    let bid =
        insert_batch_with_process(&pool, part_id, "IN_PROCESS", Some("WORKER"), proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "direct": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "不在生产架上的 IN_PROCESS 批次必须拒: {env}"
    );
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20103,
        "命中 service 层的 location 守卫（BIZ_INVALID_TRANSITION）: {env}"
    );

    // 批次一个字节都没动（守卫在任何写之前）
    let (status, location, holder, cur_proc, version): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i32,
    ) = sqlx::query_as(
        "SELECT status, location, current_holder_id, current_process_id, version \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("read batch after rejected send");
    assert_eq!(status, "IN_PROCESS", "被拒请求不得改批次状态: {env}");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert!(holder.is_none(), "不得写 holder: {env}");
    assert_eq!(cur_proc, Some(proc_id), "不得改 current_process_id: {env}");
    assert_eq!(version, 0, "被拒请求不得推 version: {env}");

    // 未建占位报价 / 未开 shipment（守卫在 resolve_direct_quote_id 之前）
    let quote_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_quote WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("count quotes");
    assert_eq!(quote_count, 0, "被拒的发送不得留下占位报价: {env}");
    let shipment_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("count shipments");
    assert_eq!(shipment_count, 0, "被拒请求不得留下 shipment: {env}");
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
    let proc_id = seed_outsource_process(&pool, "PSND", "psend", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    // 2026-10-03 新增守卫：公司必须映射该外协工序
    map_company_process(&pool, company_id, proc_id).await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
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
    let proc_id = seed_outsource_process(&pool, "PDUP", "dup", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    // 2026-10-03 新增守卫：公司必须映射该外协工序
    map_company_process(&pool, company_id, proc_id).await;

    // 2026-10-03 起工序链可选项化（无链放行、step 落 NULL）；上面的 chain + step
    // 是为了让收发路径能解析到链内 step

    // 第一次 send 成功
    let (_, env1) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
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
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
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

/// 2026-10-03：DIRECT 免审批直发 —— 无可用报价时自动建 `price=0` 的 APPROVED
/// 占位报价（`is_direct=true`），shipment 单价落 0 且请求成功。
///
/// 这条锁的是「原 501 stub 已下线」+「占位报价可被对账页识别」两件事。
#[tokio::test]
async fn send_to_outsource_direct_creates_zero_price_placeholder_quote() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Dir", "I").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "DirCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIR", "dir", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "direct": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "direct 直发: {env}");
    assert_eq!(env["data"]["status"], "OUTSOURCE");

    // 自动建的占位报价：APPROVED + price 0 + is_direct=true + note 标明来源
    let quote: (String, String, String, bool) = sqlx::query_as(
        "SELECT status, price::text, note, is_direct FROM t_outsource_quote \
         WHERE part_id = $1 AND outsource_company_id = $2 AND process_id = $3",
    )
    .bind(part_id)
    .bind(company_id)
    .bind(proc_id)
    .fetch_one(&pool)
    .await
    .expect("DIRECT 占位报价");
    assert_eq!(quote.0, "APPROVED");
    assert_eq!(
        quote.1, "0.00",
        "占位报价单价必须为 0（numeric(12,2) 文本化）"
    );
    assert!(
        quote.2.contains("DIRECT"),
        "占位报价 note 应写明来源便于对账识别，实际：{}",
        quote.2
    );
    assert!(
        quote.3,
        "占位报价必须标 is_direct=true 以避开审批报价唯一索引"
    );

    // shipment 引用该占位报价、单价 0
    let row: (i64, i32, String, String) = sqlx::query_as(
        "SELECT quote_id, quantity, status, unit_price::text FROM t_outsource_shipment \
         WHERE batch_id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("shipment row");
    let placeholder_id: i64 =
        sqlx::query_scalar("SELECT id FROM t_outsource_quote WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, placeholder_id, "shipment 必须引用占位报价");
    assert_eq!(row.1, 5);
    assert_eq!(row.2, "OUTSOURCING");
    assert_eq!(row.3, "0.00");
}

/// 2026-10-03（migration 008）：同一个 `(part_id, outsource_company_id,
/// process_id)` tuple 连发两次 DIRECT，只允许存在 **1 条** `is_direct = true` 的
/// 0 元占位报价，两张 shipment 共用同一个 `quote_id`。
///
/// 锁的是两件独立的事：
/// 1. **串行幂等**（`find_approved_quote_id` 复用路径）——第二次不新建占位报价；
/// 2. **约束真的存在**（`uq_t_outsource_quote_direct_part_company_process`）——
///    绕过 service 直接再插一条同 tuple 的占位报价必须被拒（0 行），否则并发窗口
///    （双击 / 超时重试 / 两个批次同 tuple 直发）仍会各留一条等价记录，
///    `resolve_direct_quote_id` 的 `ON CONFLICT DO NOTHING` + 回查也就形同虚设。
#[tokio::test]
async fn send_to_outsource_direct_same_tuple_keeps_single_placeholder_quote() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "DirDup", "C").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let b1 = insert_batch(&pool, part_id, "PENDING", None).await;
    // 同一 part 的第二个批次（batch_no 必须不同 —— `uq_t_part_batch_part_no`）
    let b2 = insert_nth_batch(&pool, part_id, 2, "PENDING", None, 5).await;
    let company_id = insert_outsource_company(&pool, "DirDupCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIRDUP", "dirdup", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;

    for bid in [b1, b2] {
        let (s, env) = send(
            app.clone(),
            json_request(
                "POST",
                &format!("/prod/batches/{bid}/send-to-outsource"),
                Some(json!({
                    "version": 0,
                    "outsource_company_id": company_id.to_string(),
                    "process_id": proc_id.to_string(),
                    "direct": true,
                })),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "DIRECT 直发 batch {bid}: {env}");
    }

    // 串行幂等：同 tuple 只留 1 条占位报价
    let placeholder_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_outsource_quote \
         WHERE part_id = $1 AND outsource_company_id = $2 AND process_id = $3 \
           AND status = 'APPROVED' AND is_direct = true AND deleted_at IS NULL",
    )
    .bind(part_id)
    .bind(company_id)
    .bind(proc_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        placeholder_count, 1,
        "同 (part, company, process) 只允许 1 条 DIRECT 占位报价"
    );
    // 两张 shipment 引用同一个 quote_id
    let distinct_quote_ids: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT quote_id)::bigint FROM t_outsource_shipment WHERE part_id = $1",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(distinct_quote_ids, 1, "两次 DIRECT 必须共用同一条占位报价");
    let shipment_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(shipment_count, 2, "两个批次各一张开口 shipment");

    // 约束本身生效：绕过 service 直接插同 tuple 的占位报价必须被索引拒掉
    let dup_id: i64 = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let dup = sqlx::query(
        "INSERT INTO t_outsource_quote \
             (id, part_id, outsource_company_id, process_id, price, note, status, \
              submitted_at, reviewed_at, is_direct, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 0, 'dup', 'APPROVED', now(), now(), true, now(), now()) \
         ON CONFLICT DO NOTHING",
    )
    .bind(dup_id)
    .bind(part_id)
    .bind(company_id)
    .bind(proc_id)
    .execute(&pool)
    .await
    .expect("直插重复 tuple 的 DIRECT 占位报价应被 ON CONFLICT 吞掉");
    assert_eq!(
        dup.rows_affected(),
        0,
        "uq_t_outsource_quote_direct_part_company_process 必须拒掉重复 tuple"
    );
}

/// 2026-10-03：DIRECT 命中活跃 APPROVED 报价时**复用**它（不新建占位），
/// shipment 单价等于该报价单价。
#[tokio::test]
async fn send_to_outsource_direct_reuses_active_approved_quote() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "DirR", "J").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "DirReuseCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIRREUSE", "dirreuse", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    // 已审批报价（单价 8.80，与 helper 默认 12.50 不同以示区分）
    let quote_id = insert_quote_with_price(&pool, part_id, company_id, proc_id, "8.80").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "direct": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "direct 复用: {env}");

    // 不得新建报价：整个 part 下仍只有那 1 条
    let quote_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_quote WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(quote_count, 1, "DIRECT 命中活跃报价时不得另建占位报价");
    let (shipment_quote_id, unit_price): (i64, String) = sqlx::query_as(
        "SELECT quote_id, unit_price::text FROM t_outsource_shipment WHERE batch_id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        shipment_quote_id, quote_id,
        "shipment 应引用既有 APPROVED 报价"
    );
    assert_eq!(unit_price, "8.80");
}

/// 2026-10-03：`direct=true` 与 `quote_id` 互斥（两种价来源不能同时给）。
#[tokio::test]
async fn send_to_outsource_direct_with_quote_id_rejected() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "DirX", "K").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "DirXCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIRX", "dirx", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
                "direct": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "direct+quote_id: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104);
    // 批次未被改动
    let (status,): (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "PENDING");
}

/// 2026-10-03 review 第 1 轮：`requires_approval = true` 的工序 + `direct = true`
/// → 400 / 20104，且**任何一行都不许被改**。
///
/// 守卫的必要性：改之前 `requires_approval` 只在读侧（`GET /outsource-sendable` /
/// `/outsource-pool` 的判定 SQL）生效，写侧零校验 ⇒ 绕过 UI 直接调本端点传
/// `direct=true` 就能对「先审批再发」这道业务规则该走报价的工序直发，系统里没有
/// 任何一处强制。写侧守了之后读侧/写侧才闭环：读侧决定看不看得见，写侧决定发不发
/// 得成。
///
/// 断言三件事：① 错误码是 20104（`BIZ_INVALID_VALUE`，与 `direct`/`quote_id` 互斥
/// 守卫同码 —— 前端按 20104 统一提示「参数/守卫不满足」即可）；② 批次仍是 PENDING、
/// 没有 holder / 工序写入（守卫必须落在 `mark_batch_with_status_and_meta` **之前**）；
/// ③ 没有占位报价、没有 shipment（守卫必须落在 `resolve_direct_quote_id` **之前**，
/// 否则库里会留下一条 0 元 `is_direct=true` 报价，下一次审批流程会被它污染）。
#[tokio::test]
async fn send_to_outsource_direct_rejected_when_process_requires_approval() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "ApReq", "H").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "ApReqCo").await;
    // 需审批的 OUTSOURCE 工序：写侧守卫的输入
    let proc_id = seed_outsource_process(&pool, "PAPR", "apreq", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "direct": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "需审批工序不许直发: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104, "{env}");

    // 批次未被改动（守卫在写 status 之前）
    let (status, location, holder, cur_proc): (String, Option<String>, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT status, location, current_holder_id, current_process_id \
             FROM t_part_batch WHERE id = $1",
        )
        .bind(bid)
        .fetch_one(&pool)
        .await
        .expect("read batch after rejected direct send");
    assert_eq!(status, "PENDING", "被拒请求不得改批次状态: {env}");
    assert!(location.is_none(), "不得写 holder 位置: {env}");
    assert!(holder.is_none(), "不得写 holder: {env}");
    assert!(cur_proc.is_none(), "不得写 current_process_id: {env}");

    // 未建占位报价 / 未开 shipment（守卫在 resolve_direct_quote_id 之前）
    let quote_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_quote WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(quote_count, 0, "被拒的直发不得留下占位报价: {env}");
    let shipment_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(shipment_count, 0, "被拒请求不得留下 shipment: {env}");
}

/// 2026-10-03 review 第 1 轮：同一道 `requires_approval = true` 的工序，走
/// APPROVAL（传 `quote_id`）**放行** —— 守卫只拦 `direct=true`，不能误伤正常审批流。
#[tokio::test]
async fn send_to_outsource_approval_allowed_when_process_requires_approval() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "ApOk", "O").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "ApOkCo").await;
    let proc_id = seed_outsource_process(&pool, "PAPOK", "apok", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_quote_with_price(&pool, part_id, company_id, proc_id, "33.30").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "需审批工序走 APPROVAL 必须放行: {env}");
    assert_eq!(env["data"]["status"], "OUTSOURCE", "{env}");
    let (unit_price,): (String,) =
        sqlx::query_as("SELECT unit_price::text FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("shipment row");
    assert_eq!(unit_price, "33.30", "APPROVAL 发货必须用审批价");
}

/// 2026-10-03 review 第 2 轮：需审批工序 + `quote_id` 直指一条 `is_direct=true` 的
/// APPROVED 占位报价 → 400 / 21307，且**任何一行都不许被改**。
///
/// 守卫的必要性：「需审批的工序只能凭真审批价发货」有两条入口，`direct=true` 由
/// `requires_approval` 守卫拦（见上面那条用例），`quote_id` 这条靠本守卫：占位报价
/// 是 `status='APPROVED' / is_direct=true / price=0` 的自动行，只看状态与
/// (part, company, process) 三元组时它与真审批报价无法区分。库里的占位报价来自守卫
/// 上线前的历史数据、或 `PATCH /prod/processes/{id}` 把 OUTSOURCE 工序的
/// `requires_approval` 由 false 翻成 true（该翻转为允许），所以不能靠清数据消除。
///
/// 断言四件事：① 400 / 21307（`BIZ_OUTSOURCE_QUOTE_NOT_APPROVED`，与紧邻的
/// 「非 APPROVED 不可发送」同码 —— 两者都是「这不是可用的审批价来源」）；
/// ② `t_outsource_shipment` 计数为 0（守卫必须落在 INSERT shipment 之前）；
/// ③ 批次 4 列（status / location / current_holder_id / current_process_id）未变；
/// ④ 占位报价行本身仍在且仍是 `is_direct=true`（守卫是拒请求，不是清理数据）。
#[tokio::test]
async fn send_to_outsource_rejects_direct_placeholder_quote_as_approval_price() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "PhDir", "D").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "PhDirCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIR", "pdir", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    // 预置占位报价：三元组与请求完全一致、状态 APPROVED、单价 0
    let quote_id = insert_direct_placeholder_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "占位价不得作为审批价来源: {env}"
    );
    assert_eq!(env["code"].as_i64().unwrap(), 21307, "{env}");

    let shipment_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(shipment_count, 0, "被拒请求不得留下 shipment: {env}");

    let (status, location, holder, cur_proc): (String, Option<String>, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT status, location, current_holder_id, current_process_id \
             FROM t_part_batch WHERE id = $1",
        )
        .bind(bid)
        .fetch_one(&pool)
        .await
        .expect("read batch after rejected placeholder-quote send");
    assert_eq!(status, "PENDING", "被拒请求不得改批次状态: {env}");
    assert!(location.is_none(), "不得写 holder 位置: {env}");
    assert!(holder.is_none(), "不得写 holder: {env}");
    assert!(cur_proc.is_none(), "不得写 current_process_id: {env}");

    // 占位报价行原样保留（守卫只拒请求，不改数据）
    let (still_direct, price): (bool, String) =
        sqlx::query_as("SELECT is_direct, price::text FROM t_outsource_quote WHERE id = $1")
            .bind(quote_id)
            .fetch_one(&pool)
            .await
            .expect("placeholder quote row");
    assert!(still_direct, "被拒请求不得改占位报价: {env}");
    // numeric(0) 的 text 形态是 "0.00"
    assert_eq!(price, "0.00", "{env}");
}

/// 2026-10-03 review 第 2 轮回归：非需审批工序（`requires_approval=false`）走
/// DIRECT + 复用 `is_direct=true` 占位报价**仍放行** —— 新守卫只作用于 `quote_id`
/// 路径，DIRECT 复用路径不能被误伤（否则免审批直发功能整体退化成「每次都要先审批」）。
#[tokio::test]
async fn send_to_outsource_direct_still_reuses_placeholder_quote_when_approval_not_required() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "NoAp", "Y").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "NoApCo").await;
    // 免审批工序：DIRECT 合法
    let proc_id = seed_outsource_process(&pool, "PNOAP", "noap", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_direct_placeholder_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "direct": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "免审批工序走 DIRECT 必须放行: {env}");
    assert_eq!(env["data"]["status"], "OUTSOURCE", "{env}");
    // shipment 复用同一条占位报价（不另建第二条）
    let (used_quote, unit_price): (i64, String) = sqlx::query_as(
        "SELECT quote_id, unit_price::text FROM t_outsource_shipment WHERE batch_id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("shipment row");
    assert_eq!(used_quote, quote_id, "DIRECT 必须复用预置占位报价: {env}");
    assert_eq!(unit_price, "0.00", "占位价单价为 0: {env}");
}

/// 2026-10-03：既不给 `direct` 也不给 `quote_id` → 400。
///
/// 守卫的必要性：没有价来源时 shipment 的 `unit_price` 只能落 0，而对账页看到
/// 「单价 0」无从判断是漏填还是 DIRECT 免审批直发。
#[tokio::test]
async fn send_to_outsource_without_price_source_rejected() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "NoP", "N").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "NoPCo").await;
    let proc_id = seed_outsource_process(&pool, "PNOP", "nop", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "无价来源: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104);
    let shipment_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(shipment_count, 0, "被拒请求不得留下 shipment");
}

/// 2026-10-03 新增守卫：process 类别不是 `OUTSOURCE` → 400。
///
/// 守卫前把货派给内部工序也照样成功，批次随后被标成 OUTSOURCE + 挂外协公司。
#[tokio::test]
async fn send_to_outsource_rejects_non_outsource_process_category() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Cat", "G").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "CatCo").await;
    // 内部工序（category='INHOUSE' —— t_process 的合法类别只有 INHOUSE / OUTSOURCE）
    let proc_id = seed_process(&pool, "PASM", "asm", "INHOUSE", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "非外协工序: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104);
}

/// 2026-10-03 新增守卫：公司未映射该工序 → 400（公司在册 ≠ 有该工序能力）。
#[tokio::test]
async fn send_to_outsource_rejects_company_without_process_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Map", "M").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "MapCo").await;
    let proc_id = seed_outsource_process(&pool, "PMAP", "map", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    // 刻意不调 map_company_process
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "公司未映射工序: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "PENDING", "被拒请求不得改批次状态");
}

/// 2026-10-03 新增：part **有**工艺链、但链内没有这道外协工序的 step ⇒ 仍以
/// `20702 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`（HTTP 404）拒收。
///
/// 这是「链可选」放松的**边界用例**，与 `send_and_receive_without_process_chain_succeeds`
/// 构成对照：两条放行/拒收的判据只有 `process_chain_id` 是否为 NULL 这一个区别。
/// 跟着「没链就放行」把 20702 一起吞掉的后果是：批次带着一个链内不存在的工序静默
/// 入池，之后每一步的 step 定位全部漂移，且没有任何报错可查 —— 这是真数据错误
/// （链存在却没登记正在加工的工序），不属于「旧零件没制定工序链」的兼容范畴。
#[tokio::test]
async fn send_to_outsource_rejects_process_missing_from_existing_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Step", "T").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "StepCo").await;
    let proc_id = seed_outsource_process(&pool, "PSTP", "step", true).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    // 链里只登记**另一道**工序（INHOUSE），外协工序 proc_id 刻意不入链
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let other = seed_process(&pool, "PSTP-OTH", "oth", "INHOUSE", false).await;
    create_step(&pool, chain_id, other, 1).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "链内无该工序必须拒: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20702);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "PENDING", "被拒请求不得改批次状态");
}

/// 2026-10-03 部分发送：`quantity = 批次量的一半` → 源批次留在原处（量减半、
/// 状态/货架不变），新子批次 OUTSOURCE，shipment 记本次发送量。
///
/// 锁住的是「拆批而不是静默整批」：`quantity` 缺省即整批，所以显式的部分量必须真的
/// 走拆批路径，否则界面上选 5 件、实际整批发出。
#[tokio::test]
async fn send_to_outsource_partial_quantity_splits_batch() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Psend", "B").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    // 源状态用 PENDING：本用例只覆盖部分发送的拆批语义，与源状态无关；
    // `IN_PROCESS → OUTSOURCE` 那条边的守卫（`IN_PROCESS` 批次必须在
    // PRODUCTION_SHELF 上）由同文件的配对负向用例覆盖。
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "PsendCo").await;
    let proc_id = seed_outsource_process(&pool, "PPSEND", "psend2", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
                "quantity": 2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "部分发送: {env}");

    let batches = list_batches(&pool, part_id).await;
    assert_eq!(batches.len(), 2, "部分发送应拆出 1 个子批次：{batches:?}");
    let source = batches.iter().find(|b| b.0 == bid).expect("源批次仍在");
    assert_eq!(source.1, 3, "源批次余量 = 5 - 2");
    assert_eq!(source.2, "PENDING", "源批次状态不变（没被发出去）");
    assert_eq!(source.3, None, "源批次 location 不变（仍留在原处）");
    let child = batches.iter().find(|b| b.0 != bid).expect("子批次");
    assert_eq!(child.1, 2);
    assert_eq!(child.2, "OUTSOURCE");
    assert_eq!(child.3.as_deref(), Some("OUTSOURCE_COMPANY"));

    // shipment 挂在**子批次**上、quantity = 本次发送量
    let (ship_batch, ship_qty, ship_price): (i64, i32, String) = sqlx::query_as(
        "SELECT batch_id, quantity, unit_price::text FROM t_outsource_shipment \
         WHERE part_id = $1",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ship_batch, child.0, "shipment 应挂在发出去的子批次上");
    assert_eq!(ship_qty, 2);
    assert_eq!(ship_price, "12.50");

    // OCC 契约（2026-10-03 补断言）：源批次的 version 被 `_split_batch_inner` 的
    // `version = version + 1` 顶到 1；子批次 INSERT 写死 version 0，随后 status_gate
    // 流转 +1 → 也是 1。二次部分发送必须拿刷新后的 version=1。
    assert_eq!(source.4, 1, "拆批后源批次 version +1");
    assert_eq!(child.4, 1, "子批次 0 → 1（INSERT 写 0 + status_gate +1）");

    // 派生契约：min-progress 里源批次 `PENDING`(rank 0) 慢于子批次
    // `OUTSOURCE`(rank 3)，故 part 仍 `PENDING`。
    assert_eq!(part_status(&pool, part_id).await, "PENDING");

    // part_event 记本次发送量与子批次
    let (ev_batch, ev_qty): (i64, i32) = sqlx::query_as(
        "SELECT batch_id, quantity FROM t_part_event \
         WHERE part_id = $1 AND event_type = 'SENT_TO_OUTSOURCE'",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ev_batch, child.0);
    assert_eq!(ev_qty, 2);
}

/// 2026-10-03：部分发送时子批次的 OCC 锚必须是**读回行**的 version，不能拿请求里的
/// `req.version` 顶替 —— 子批次被 `_split_batch_inner` 写死 `version = 0`，拿一个
/// 非 0 的 `req.version` 去撞必然 0 行。
///
/// 用例把源批次 version 预置成 2（真实场景：批次已经流转过若干次），这样两个 version
/// 值才真的不同 —— 若用 version=0 的批次，`req.version` 恰好等于子批次的 0，错实现
/// 也能蒙混过关，本用例就失去鉴别力。
#[tokio::test]
async fn send_to_outsource_partial_anchors_child_on_read_back_version() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "PVer", "L").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    // 源批次已流转过若干次（version 2），与子批次的 0 明确不同
    sqlx::query("UPDATE t_part_batch SET version = 2 WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .expect("预置源批次 version=2");
    let company_id = insert_outsource_company(&pool, "PVerCo").await;
    let proc_id = seed_outsource_process(&pool, "PPVER", "pver", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 2,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
                "quantity": 2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "部分发送（源批次 version=2）: {env}");

    let batches = list_batches(&pool, part_id).await;
    assert_eq!(batches.len(), 2, "部分发送应拆出 1 个子批次：{batches:?}");
    let source = batches.iter().find(|b| b.0 == bid).expect("源批次");
    let child = batches.iter().find(|b| b.0 != bid).expect("子批次");
    assert_eq!(source.4, 3, "源批次 2 → 3（拆批 +1）");
    assert_eq!(child.4, 1, "子批次 0 → 1（读回行 version=0 作锚）");
}

/// 2026-10-03：`quantity == 批次量` 视为整批，**不产生**子批次。
#[tokio::test]
async fn send_to_outsource_quantity_equal_batch_is_whole_batch() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "EqAll", "E").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "EqAllCo").await;
    let proc_id = seed_outsource_process(&pool, "PEQALL", "eqall", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 0,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
                "quantity": 5,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "整批发送: {env}");
    let batches = list_batches(&pool, part_id).await;
    assert_eq!(batches.len(), 1, "quantity == 批次量不得拆批：{batches:?}");
    assert_eq!(batches[0].0, bid);
    assert_eq!(batches[0].1, 5);
    assert_eq!(batches[0].2, "OUTSOURCE");
}

/// 2026-10-03：部分发送的 3 类数量非法（超量 / 0 / 负数）一律 400。
#[tokio::test]
async fn send_to_outsource_invalid_quantity_rejected() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "BadQ", "X").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "BadQCo").await;
    let proc_id = seed_outsource_process(&pool, "PBADQ", "badq", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    for bad in [6, 0, -1] {
        let (s, env) = send(
            app.clone(),
            json_request(
                "POST",
                &format!("/prod/batches/{bid}/send-to-outsource"),
                Some(json!({
                    "version": 0,
                    "outsource_company_id": company_id.to_string(),
                    "process_id": proc_id.to_string(),
                    "quote_id": quote_id.to_string(),
                    "quantity": bad,
                })),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "quantity={bad}: {env}");
        assert_eq!(env["code"].as_i64().unwrap(), 20104);
    }
    let (status, qty): (String, i32) =
        sqlx::query_as("SELECT status, quantity FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "PENDING");
    assert_eq!(qty, 5, "被拒请求不得拆批改量");
}

/// 2026-10-03：部分发送的乐观锁仍锚在**源批次**上（过期 version → 409）。
#[tokio::test]
async fn send_to_outsource_partial_stale_version_conflicts() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Stale", "T").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "StaleCo").await;
    let proc_id = seed_outsource_process(&pool, "PSTALE", "stale", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
                "version": 99,
                "outsource_company_id": company_id.to_string(),
                "process_id": proc_id.to_string(),
                "quote_id": quote_id.to_string(),
                "quantity": 2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "过期 version: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40901);
    let batches = list_batches(&pool, part_id).await;
    assert_eq!(batches.len(), 1, "OCC 失败不得留下子批次");
}

#[tokio::test]
async fn send_to_outsource_quote_not_approved_returns_21307() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Qd", "Q").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "QdCo").await;
    let proc_id = seed_outsource_process(&pool, "PQ", "pq", true).await;
    // 直接 raw SQL 插一个 DRAFT quote（不走 service 校验）

    // 2026-10-03 起工序链可选项化（无链放行、step 落 NULL）；建链 + step 是为了
    // 让收发路径能解析到链内 step
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    // 2026-10-03 新增守卫：公司必须映射该外协工序
    map_company_process(&pool, company_id, proc_id).await;
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
            &format!("/prod/batches/{bid}/send-to-outsource"),
            Some(json!({
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
    let proc_id = seed_outsource_process(&pool, "PR", "pr", true).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    // 2026-10-03 起工序链可选项化（无链放行、step 落 NULL）；建链 + step 是为了
    // 让收发路径能解析到链内 step
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
    let next_proc = seed_outsource_process(&pool, "REC-PROC", "recv_proc", true).await;
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
    // 2026-09-30：入池归属判定已改走 `current_process_id`（不需要 step 行），
    // 但 `ProcessChainRepo::resolve_step_id_by_process` 守卫仍要求 chain 内存在
    // 活跃 step，否则端点会以 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND 422 拒绝，
    // 故本 fixture 的 create_step 必须保留。
    let next_step_id = create_step(&pool, chain_id, next_proc, 2).await;
    let _ = next_step_id; // 确认 step 已落库；service 内自行解析

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource"),
            Some(json!({
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
    // 2026-10-03 补断言：整批回收必须落 received_at（部分接收**不**落，
    // 见 receive_from_outsource_partial_quantity_keeps_shipment_open）
    let (status, received_at): (String, Option<chrono::NaiveDateTime>) =
        sqlx::query_as("SELECT status, received_at FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "RECEIVED");
    assert!(received_at.is_some(), "整批回收必须写 received_at");
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

/// 2026-10-03 部分接收：子批次回生产架、源批次保留余量且**仍 OUTSOURCE**，
/// 开口 shipment 保持 `OUTSOURCING` / `received_at` 仍 NULL，且不写 quote
/// event RECEIVED。
///
/// 这是本任务最容易被"顺手改成整批"的一处语义，锁死它。
#[tokio::test]
async fn receive_from_outsource_partial_quantity_keeps_shipment_open() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, quote_id, shelf_id, next_proc) =
        setup_inflight(&pool, "PRec", "Y", "PPRec", "REC-P", "REC-PROC-P", 5).await;
    let part_id: i64 = sqlx::query_scalar("SELECT part_id FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource"),
            Some(json!({
                "version": 0,
                "shelf_id": shelf_id.to_string(),
                "next_process_id": next_proc.to_string(),
                "quantity": 2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "部分接收: {env}");

    let batches = list_batches(&pool, part_id).await;
    assert_eq!(batches.len(), 2, "部分接收应拆出 1 个子批次：{batches:?}");
    let source = batches.iter().find(|b| b.0 == bid).expect("源批次");
    assert_eq!(source.1, 3, "源批次余量 = 5 - 2");
    assert_eq!(source.2, "OUTSOURCE", "源批次仍在外协厂");
    assert_eq!(source.3.as_deref(), Some("OUTSOURCE_COMPANY"));
    let child = batches.iter().find(|b| b.0 != bid).expect("子批次");
    assert_eq!(child.1, 2);
    assert_eq!(child.2, "IN_PROCESS");
    assert_eq!(child.3.as_deref(), Some("PRODUCTION_SHELF"));

    // 记账口径：shipment 仍开口
    let (status, received_at, qty, batch_id): (
        String,
        Option<chrono::NaiveDateTime>,
        i32,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, received_at, quantity, batch_id FROM t_outsource_shipment \
         WHERE part_id = $1",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "OUTSOURCING", "部分接收不得关 shipment");
    assert!(received_at.is_none(), "部分接收不得写 received_at");
    assert_eq!(qty, 5, "shipment 记的是发出时的全量");
    assert_eq!(batch_id, Some(bid), "shipment 仍挂在源批次上");

    // 不写 quote event RECEIVED
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_outsource_quote_event \
         WHERE quote_id = $1 AND event_type = 'RECEIVED'",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(event_count, 0, "部分接收不得写 RECEIVED 事件");

    // part_event 记本次回收量与子批次
    let (ev_batch, ev_qty): (i64, i32) = sqlx::query_as(
        "SELECT batch_id, quantity FROM t_part_event \
         WHERE part_id = $1 AND event_type = 'RECEIVED_FROM_OUTSOURCE'",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ev_batch, child.0);
    assert_eq!(ev_qty, 2);

    // OCC 契约（2026-10-03 补断言）：源批次 version 0 → 1（`_split_batch_inner` 的
    // `version = version + 1`），子批次 0 → 1（INSERT 写 0 + status_gate +1）。
    // 二次部分接收必须拿刷新后的 version=1。
    assert_eq!(source.4, 1, "拆批后源批次 version +1");
    assert_eq!(child.4, 1, "子批次 0 → 1");

    // 派生契约：源批次 `OUTSOURCE`(rank 3) 与子批次 `IN_PROCESS`(rank 2) 并存时
    // min-progress 取 2 → part 变 `IN_PROCESS`（尽管源批次还在外协厂）。
    assert_eq!(part_status(&pool, part_id).await, "IN_PROCESS");
}

/// 2026-10-03：部分接收 → 再整批回收余量，这条链上
/// `uq_t_outsource_shipment_open_batch` 的不变式是「**同一批次同时最多一张开口
/// shipment**」：部分接收期间开口不关，第二次整批回收才关它，且此时 quote event
/// `RECEIVED` 只写一次。
///
/// 顺带锁住拆批的 OCC 副作用：第一次部分接收把源批次 version 顶到 1，第二次请求
/// **必须**带刷新后的 1。
#[tokio::test]
async fn receive_from_outsource_partial_then_whole_closes_shipment() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, quote_id, shelf_id, next_proc) =
        setup_inflight(&pool, "TwiceR", "F", "PTWICE", "REC-F", "REC-PROC-F", 5).await;
    let part_id: i64 = sqlx::query_scalar("SELECT part_id FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    // 源批次 version 预置成 2：与子批次的 0 明确不同，拆批时若错拿 `req.version`
    // 当子批次的 OCC 锚，本用例会直接 409（鉴别力来源）
    sqlx::query("UPDATE t_part_batch SET version = 2 WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .expect("预置源批次 version=2");

    // 第一次：部分回收 2 件
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource"),
            Some(json!({
                "version": 2,
                "shelf_id": shelf_id.to_string(),
                "next_process_id": next_proc.to_string(),
                "quantity": 2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "部分接收: {env}");
    let (status, received_at): (String, Option<chrono::NaiveDateTime>) =
        sqlx::query_as("SELECT status, received_at FROM t_outsource_shipment WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "OUTSOURCING", "部分接收后开口 shipment 仍开口");
    assert!(received_at.is_none());

    // 拆批把源批次 version 顶到 3；第二次请求必须带 3
    let source_version: i32 = sqlx::query_scalar("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source_version, 3, "拆批后源批次 version 2 → 3");

    // 第二次：整批回收剩余 3 件（quantity == 余量 → 不再拆批）
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource"),
            Some(json!({
                "version": 3,
                "shelf_id": shelf_id.to_string(),
                "next_process_id": next_proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "整批回收余量: {env}");

    // 开口 shipment 此刻才关
    let (status, received_at, ver): (String, Option<chrono::NaiveDateTime>, i32) = sqlx::query_as(
        "SELECT status, received_at, version FROM t_outsource_shipment WHERE part_id = $1",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "RECEIVED", "整批回收后开口 shipment 必须关闭");
    assert!(received_at.is_some(), "整批回收必须写 received_at");
    assert_eq!(ver, 1, "shipment version 0 → 1（关单那一次 UPDATE）");

    // 仍然只有 1 张 shipment（`uq_t_outsource_shipment_open_batch` 不允许第二张开口）
    let shipment_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(shipment_count, 1, "全程只应有 1 张 shipment");

    // quote event RECEIVED 只在整批回收时写一次
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_outsource_quote_event \
         WHERE quote_id = $1 AND event_type = 'RECEIVED'",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(event_count, 1, "RECEIVED 事件只应有 1 条");

    // 两条批次都回生产架，part 派生 `IN_PROCESS`
    let batches = list_batches(&pool, part_id).await;
    assert_eq!(batches.len(), 2, "第二次不再拆批：{batches:?}");
    for b in &batches {
        assert_eq!(b.2, "IN_PROCESS", "两条批次都应回生产架：{batches:?}");
        assert_eq!(b.3.as_deref(), Some("PRODUCTION_SHELF"));
    }
    let source_after: i32 = sqlx::query_scalar("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source_after, 4, "整批回收把源批次 3 → 4（status_gate +1）");
    assert_eq!(part_status(&pool, part_id).await, "IN_PROCESS");
}

/// 2026-10-03 负向用例：拆批把源批次 version +1 之后，用**旧** version 再回收一次
/// 必须 409。这条锁住「二次收发必须先刷新列表」这条调用方契约。
#[tokio::test]
async fn receive_from_outsource_partial_then_stale_version_conflicts() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _q, shelf_id, next_proc) =
        setup_inflight(&pool, "StaleTwice", "H", "PSTALE", "REC-H", "REC-PROC-H", 5).await;

    let (s, _env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource"),
            Some(json!({
                "version": 0,
                "shelf_id": shelf_id.to_string(),
                "next_process_id": next_proc.to_string(),
                "quantity": 2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "第一次部分接收");

    // 复用 version=0（未刷新列表）→ 409
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource"),
            Some(json!({
                "version": 0,
                "shelf_id": shelf_id.to_string(),
                "next_process_id": next_proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "拆批后旧 version 必须 409: {env2}"
    );
    assert_eq!(env2["code"].as_i64().unwrap(), 40901);
}

/// 2026-10-03：部分接收的 3 类数量非法（超量 / 0 / 负数）一律 400 且不动批次。
#[tokio::test]
async fn receive_from_outsource_invalid_quantity_rejected() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _q, shelf_id, next_proc) =
        setup_inflight(&pool, "BadR", "W", "PBADR", "REC-B", "REC-PROC-B", 5).await;

    for bad in [6, 0, -1] {
        let (s, env) = send(
            app.clone(),
            json_request(
                "POST",
                &format!("/prod/batches/{bid}/receive-from-outsource"),
                Some(json!({
                    "version": 0,
                    "shelf_id": shelf_id.to_string(),
                    "next_process_id": next_proc.to_string(),
                    "quantity": bad,
                })),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "quantity={bad}: {env}");
        assert_eq!(env["code"].as_i64().unwrap(), 20104);
    }
    let (status, qty): (String, i32) =
        sqlx::query_as("SELECT status, quantity FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "OUTSOURCE");
    assert_eq!(qty, 5, "被拒请求不得拆批改量");
}

/// 2026-10-03：部分接收的乐观锁同样锚在源批次上（过期 version → 409）。
#[tokio::test]
async fn receive_from_outsource_partial_stale_version_conflicts() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _q, shelf_id, next_proc) =
        setup_inflight(&pool, "StaleR", "V", "PSTALER", "REC-S", "REC-PROC-S", 5).await;
    let part_id: i64 = sqlx::query_scalar("SELECT part_id FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource"),
            Some(json!({
                "version": 99,
                "shelf_id": shelf_id.to_string(),
                "next_process_id": next_proc.to_string(),
                "quantity": 2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "过期 version: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40901);
    assert_eq!(
        list_batches(&pool, part_id).await.len(),
        1,
        "OCC 失败不得拆批"
    );
}

/// 回归：`receive-from-outsource-to-inspection`（整批直送品检）不受本次改动影响
/// —— 仍落 `INSPECTION` + 送检架，并把开口 shipment 标 RECEIVED。
#[tokio::test]
async fn receive_from_outsource_to_inspection_closes_shipment() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, quote_id, _shelf, _next_proc) =
        setup_inflight(&pool, "Insp", "Z", "PINSP", "REC-I", "REC-PROC-I", 5).await;
    let insp_shelf = insert_shelf(&pool, "INS-1", "INSPECTION").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/receive-from-outsource-to-inspection"),
            Some(json!({
                "version": 0,
                "shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "直送品检: {env}");
    assert_eq!(env["data"]["status"], "INSPECTION");
    let (status, location, received_at): (String, Option<String>, Option<chrono::NaiveDateTime>) =
        sqlx::query_as(
            "SELECT b.status, b.location, s.received_at FROM t_part_batch b \
             JOIN t_outsource_shipment s ON s.batch_id = b.id WHERE b.id = $1",
        )
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "INSPECTION");
    assert_eq!(location.as_deref(), Some("INSPECTION_SHELF"));
    assert!(
        received_at.is_some(),
        "整批直送品检应关 shipment 并写 received_at"
    );
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_outsource_quote_event \
         WHERE quote_id = $1 AND event_type = 'RECEIVED'",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(event_count, 1);
}

#[tokio::test]
async fn reconcile_update_shipment_unit_price_quantity() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "RU", "U").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "RecCo").await;
    let proc_id = seed_outsource_process(&pool, "PRU", "pru", true).await;

    // 2026-10-03 起工序链可选项化（无链放行、step 落 NULL）；建链 + step 是为了
    // 让收发路径能解析到链内 step
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
    // 2026-10-03：`shipment_out` 的 `customer_path` 改为真算（`part_customer_names`
    // + `join_customer_path`），不再硬编码 `None`；此处断言它与 sent-parts list
    // 侧口径一致 —— 这条 L1 客户无 parent，故只给 L2 名。
    assert_eq!(
        env["data"]["customer_path"], "RU",
        "reconcile-update 出参的 customer_path 必须真算: {env}"
    );

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
