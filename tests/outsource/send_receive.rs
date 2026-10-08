//! `POST /api/v2/outsource-queue/move` 三合一移动端点集成测试（2026-10-09 新增）
//!
//! 取代本文件 2026-10-09 之前的三个单边端点用例（`POST /prod/batches/{id}/
//! send-to-outsource` / `receive-from-outsource` /
//! `receive-from-outsource-to-inspection`），一并覆盖 shipment 记账、价来源守卫与
//! 三合一新增的 `from` 锚点守卫。
//!
//! ## 覆盖面（三条路径 + 出参契约 + 守卫矩阵）
//! - **发送**（`PRODUCTION_SHELF → OUTSOURCE_COMPANY`）：shipment INSERT +
//!   quote event `SENT` + part 事件 `SENT_TO_OUTSOURCE`；`IN_PROCESS` / `PENDING`
//!   两个源状态；`IN_PROCESS` 但不在生产架 → 20103
//! - **回收生产**（`OUTSOURCE_COMPANY → PRODUCTION_SHELF`）：shipment 标
//!   `RECEIVED` + `received_at` + quote event `RECEIVED`；工序**推进**到
//!   `next_process_id`；省略 `next_process_id` 时按工序链推导 / 推不出返 20706
//! - **回收品检**（`OUTSOURCE_COMPANY → INSPECTION_SHELF`）：2026-10-10 起该方向
//!   **整条下线**（`OutsourceLocation::InspectionShelf` 变体删除），发这个 kind 的请求
//!   在反序列化阶段即被拒 ⇒ 422 纯文本、不进 `R<T>` 信封、批次零改动
//! - **出参契约**：5 个雪花 id 全是字符串；`shipment_id` / `new_process_id` 在非本方向
//!   **键不存在**
//! - **请求形状守卫**：同 kind → 40001（且早于查批次）、`from` 与真实位置/holder 不符
//!   → 20122、`version` 过期 → 40901、批次不存在 → 20109、非发送方向带 `quote_id` /
//!   `direct` → 20104、**漏传 `version` → 422 纯文本**
//! - **回收方向的源状态白名单**：批次还在生产架上（`IN_PROCESS`）却请求回收 → 20103
//!   （这条守卫是 match 兜底分支不 panic 的前提）
//! - **价来源守卫**：APPROVAL / DIRECT 二选一、`requires_approval` 工序不许直发、
//!   占位价不能当审批价（21307）、DRAFT 报价 21307、公司不存在 21201 / 停用 21205、
//!   非 OUTSOURCE 工序 20104、公司未映射工序 20104、链内缺该工序 step 20702
//! - **DIRECT 占位报价幂等**（migration 008：同 tuple 只留 1 条 `is_direct=true`）
//!
//! ## 2026-10-09 删除的用例（部分发送 / 部分接收）
//! move 端点是**整批**语义（入参没有 `quantity`），部分流转走共用拆批端点
//! `POST /api/v2/batches/split`。随之删除的用例：
//! `send_to_outsource_partial_*`（5 条）、`send_to_outsource_quantity_equal_batch_*`、
//! `send_to_outsource_invalid_quantity_*`、`receive_from_outsource_partial_*`（5 条）。
//! 拆批端点自身的用例在 `tests/production/` 侧。
//!
//! ## 一个必须知道的形态收窄
//! 发送的 `from.kind` 恒为 `PRODUCTION_SHELF`，且 `from.shelf_id` 必须等于批次真实
//! `current_holder_id` ⇒ **未上架的 `PENDING` 批次（`location IS NULL`）不再能直接发
//! 外协**，要先 `place-on-shelf`。这是看板候选卡形态决定的（`shelf_id` 对这类行序列成
//! 空串、`can_send` 不因此为 false 但写端点必拒），旧端点允许直接从 PENDING 发是绕过
//! 看板的旁路。
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 删除本地 `send` / `json_request` / `setup` / `login_manager` 通用 helper，统一走
//! `use hsh_erp_test_support::{...}` + `bootstrap_as_manager()` +
//! `load_outsource_fixture(&pool)`。域独享 helper（customer / part / batch / company /
//! process / quote / chain / shelf / mapping）用 `sqlx::query` 直插，与
//! `fixtures::*` 同形 SQL。
//!
//! ## 不预置 t_part / t_part_batch / t_part_process_chain / t_process_chain_step /
//!  t_outsource_quote / t_outsource_shipment
//! 状态机不允许 part 从 OUTSOURCE 回退 PENDING；每个测试要按需造不同
//! (part, batch, company, process, quote) 组合 + 自建 chain/step。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, pool_snowflake, send,
    send_raw, test_app, test_pool, test_state,
};

/// 移动端点路径（三合一后是静态段，主键走 body）。
const MOVE_PATH: &str = "/outsource-queue/move";

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 outsource fixture + 以 MANAGER 身份登录。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  move 请求体构造（三条路径各一个）
// ===========================================================================

/// 发送方向 body：`from` = 生产架 → `to` = 外协公司。
fn send_body(batch_id: i64, version: i32, shelf_id: i64, company_id: i64) -> Value {
    json!({
        "batch_id": batch_id.to_string(),
        "version": version,
        "from": { "kind": "PRODUCTION_SHELF", "shelf_id": shelf_id.to_string() },
        "to": { "kind": "OUTSOURCE_COMPANY", "company_id": company_id.to_string() },
    })
}

/// 发送方向 body + APPROVAL 价来源（`quote_id`）。
fn send_body_with_quote(
    batch_id: i64,
    version: i32,
    shelf_id: i64,
    company_id: i64,
    quote_id: i64,
) -> Value {
    let mut body = send_body(batch_id, version, shelf_id, company_id);
    body["quote_id"] = json!(quote_id.to_string());
    body
}

/// 发送方向 body + DIRECT 价来源（`direct = true`）。
fn send_body_direct(batch_id: i64, version: i32, shelf_id: i64, company_id: i64) -> Value {
    let mut body = send_body(batch_id, version, shelf_id, company_id);
    body["direct"] = json!(true);
    body
}

/// 回收方向 body：`from` = 外协公司 → `to` = 生产架，`next_process_id` 可省略
/// （`None` ⇒ 不带该键，让后端按工序链推导）。
fn receive_body(
    batch_id: i64,
    version: i32,
    company_id: i64,
    shelf_id: i64,
    next_process_id: Option<i64>,
) -> Value {
    let mut to = json!({ "kind": "PRODUCTION_SHELF", "shelf_id": shelf_id.to_string() });
    if let Some(pid) = next_process_id {
        to["next_process_id"] = json!(pid.to_string());
    }
    json!({
        "batch_id": batch_id.to_string(),
        "version": version,
        "from": { "kind": "OUTSOURCE_COMPANY", "company_id": company_id.to_string() },
        "to": to,
    })
}

// ===========================================================================
//  本文件独享 helpers（绕开 fixtures::* 因为 Phase H gate 5 禁止从
//  `fixtures` 模块 use 任何动态 helper）
// ===========================================================================

/// 直插用的雪花 ID：走 `test-support::pool_snowflake()`（**进程级**
/// `OnceLock<Mutex<..>>`，instance 由 pid ⊕ 启动时间派生）。
///
/// 2026-10-09 换掉「每次 `SnowflakeIdGenerator::new(epoch, 1)`」的写法：新建的生成器
/// 在同一毫秒内连续两次调用会生成**完全相同**的 id（instance 相同 + 时间戳相同 + seq 都
/// 从 0 开始），撞 `t_*_pkey`，更隐蔽的是撞成「shelf_id == process_id」这类业务列 ——
/// DB 的 `ck_*_no_self_loop` CHECK 会以一条与被测逻辑无关的约束错误把用例打断。
/// 范本与理由见 `tests/outsource/pool.rs::next_id`。
fn next_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
}

/// 直插 L1 客户（绕开 customer CRUD）。
async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let id = next_id();
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
    let id = next_id();
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
    insert_nth_batch(pool, part_id, 1, status, location, 5).await
}

/// 直插同 part 下的**第 n 个**批次（`uq_t_part_batch_part_no` 要求 batch_no 互异）。
async fn insert_nth_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    status: &str,
    location: Option<&str>,
    qty: i32,
) -> i64 {
    let id = next_id();
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

/// 直插带 `current_process_id` 的批次（候��卡 / 在途卡的形态基础）。
///
/// `holder_id` 必须是该批次真实 `current_holder_id` —— move 端点的 `from` 守卫拿它
/// 逐字比对（`20122`），造数据时必须一次给对，否则用例会挂在守卫上而不是它自己声称
/// 验证的那条路径上。
async fn insert_batch_with_process(
    pool: &PgPool,
    part_id: i64,
    status: &str,
    location: Option<&str>,
    current_process_id: i64,
    holder_id: Option<i64>,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch \
         (id, part_id, batch_no, quantity, status, location, current_process_id, \
          current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 5, $3, $4, $5, $6, 0, $7, $7)",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(location)
    .bind(current_process_id)
    .bind(holder_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch with current_process_id");
    id
}

/// 直插外协公司（`is_active` 可指定，验 21205 时用 `false`）。
async fn insert_outsource_company_with_active(pool: &PgPool, name: &str, is_active: bool) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company (id, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(is_active)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company");
    id
}

/// 直插外协公司（启用）。
async fn insert_outsource_company(pool: &PgPool, name: &str) -> i64 {
    insert_outsource_company_with_active(pool, name, true).await
}

/// 直插任意 category 的 process。
///
/// `requires_approval` 决定两件事 —— 写侧 move 端点拒
/// `requires_approval=true` + `direct=true`（20104），读侧候选卡判定该
/// (part, process) 需不需要先有审批报价。DIRECT 用例必须显式传 `false`，否则它会挂在
/// 「直发被拒」而不是它自己声称验证的那条路径上。
async fn seed_process(
    pool: &PgPool,
    code: &str,
    name: &str,
    category: &str,
    requires_approval: bool,
) -> i64 {
    let id = next_id();
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

/// 直插 `t_outsource_company_process`（公司 ↔ 工序映射）。
///
/// move 发送方向有「公司必须映射该外协工序」守卫（`20104`），所以每个发外协用例都要
/// 调本函数；守卫本身由 `move_send_rejects_company_without_process_mapping` 锁住。
async fn map_company_process(pool: &PgPool, company_id: i64, process_id: i64) -> i64 {
    let id = next_id();
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

/// 直插 `is_direct = true` 的 APPROVED 报价，即 `resolve_direct_quote_id` 自动建的
/// 「免审批直发占位价」（`price` 通常 0）。
///
/// 只能直插而不能靠调端点造出来：需审批工序上的 DIRECT 已被写侧守卫拒，而真实数据里
/// 这批行来自守卫上线之前的历史数据、或 `requires_approval` 由 false 翻成 true 的存量
/// 工序。
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
    let id = next_id();
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

/// 为 part 建工序链并绑到 part 上（返回 chain_id）。
///
/// 工序链**不是**发送 / 回收的前提（无链放行、`current_process_step_id` 落 NULL），
/// 但一旦有链，链内缺该工序的 step 会被 `optional_step_id` 以 20702 拒收 —— 两类用例
/// 都要用到本 helper。
async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    let chain_id = next_id();
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

/// 在指定 chain 内创建 step（process_id + sort_order），返回 step_id。
async fn create_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    let step_id = next_id();
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
    let id = next_id();
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

/// 直插 `t_shelf_process`（货架 ↔ 工序映射）。回收到生产架时缺它 ⇒ 该工序无可用生产货架
/// ⇒ 20508（选架的候选集要求「映射了该工序」，候选为空即选不出）。
async fn map_shelf_process(pool: &PgPool, shelf_id: i64, process_id: i64) {
    let id = next_id();
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

/// 回收侧成套 fixture：PRODUCTION 货架 + 映射「下一道工序」+ 在 part 的 chain 里为该
/// 工序建 step（`optional_step_id` 守的就是那条 step，缺了会以 20702 拒收）。
///
/// 返回 `(shelf_id, next_process_id)`。
async fn setup_receive_side(
    pool: &PgPool,
    chain_id: Option<i64>,
    shelf_code: &str,
    next_proc_code: &str,
) -> (i64, i64) {
    let shelf_id = insert_shelf(pool, shelf_code, "PRODUCTION").await;
    let next_proc = seed_outsource_process(pool, next_proc_code, "recv_proc", true).await;
    map_shelf_process(pool, shelf_id, next_proc).await;
    if let Some(chain_id) = chain_id {
        create_step(pool, chain_id, next_proc, 2).await;
    }
    (shelf_id, next_proc)
}

/// 外协在途态的成套 fixture：
/// - part（PENDING 源、无链）
/// - `OUTSOURCE` + `OUTSOURCE_COMPANY` + holder=公司 + `current_process_id=外协工序`
///   的批次（`from` 守卫要 holder 逐字相等）
/// - 开口 `OUTSOURCING` shipment（数量 = 发出时的全量 = `batch_qty`）
/// - 回收目标架（PRODUCTION）+ 映射 + chain step
///
/// `chain_id: Option<i64>`：`Some` ⇒ 建链并为外协工序 + 下一道工序各建一个 step
/// （推导用例需要）；`None` ⇒ 无链（20706 用例需要）。
///
/// 返回 `(batch_id, part_id, company_id, quote_id, shelf_id, next_process_id, step_id)`。
#[allow(clippy::type_complexity)]
async fn setup_inflight(
    pool: &PgPool,
    name: &str,
    prefix: &str,
    proc_code: &str,
    shelf_code: &str,
    next_proc_code: &str,
    with_chain: bool,
) -> (i64, i64, i64, i64, i64, i64, Option<i64>) {
    let customer_id = insert_l1_customer(pool, name, prefix).await;
    let part_id = insert_part(pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(pool, &format!("{name}Co")).await;
    let proc_id = seed_outsource_process(pool, proc_code, "proc", true).await;
    let quote_id = insert_approved_quote(pool, part_id, company_id, proc_id).await;
    let bid = insert_batch_with_process(
        pool,
        part_id,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        proc_id,
        Some(company_id),
    )
    .await;
    let chain_id = if with_chain {
        let chain_id = create_chain_for_part(pool, part_id).await;
        let step_id = create_step(pool, chain_id, proc_id, 1).await;
        // 锚 step 指针（推导 SQL 的 `cur.id = current_process_step_id` 靠它取链）
        sqlx::query("UPDATE t_part_batch SET current_process_step_id = $2 WHERE id = $1")
            .bind(bid)
            .bind(step_id)
            .execute(pool)
            .await
            .expect("bind batch step pointer");
        Some(chain_id)
    } else {
        None
    };
    let now = now_naive();
    let shipment_id: i64 = next_id();
    sqlx::query(
        "INSERT INTO t_outsource_shipment \
         (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
          quantity, unit_price, status, sent_at, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, 5, 12.50, 'OUTSOURCING', $7, 0, $7, $7)",
    )
    .bind(shipment_id)
    .bind(quote_id)
    .bind(part_id)
    .bind(bid)
    .bind(company_id)
    .bind(proc_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_shipment");
    let (shelf_id, next_proc) =
        setup_receive_side(pool, chain_id, shelf_code, next_proc_code).await;
    (
        bid,
        part_id,
        company_id,
        quote_id,
        shelf_id,
        next_proc,
        chain_id.map(|_| proc_id),
    )
}

/// 读批次当前 `version`（下一次 move 的 OCC 锚）。
async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("read t_part_batch.version")
}

/// 读 `t_part` 的派生 `status`（批次 min-progress 派生的缓存列）。
async fn part_status(pool: &PgPool, part_id: i64) -> String {
    sqlx::query_scalar("SELECT status FROM t_part WHERE id = $1")
        .bind(part_id)
        .fetch_one(pool)
        .await
        .expect("read t_part.status")
}

/// 发一次 move，返回 `(status, envelope)`（函数名避开 Rust 关键字 `move`）。
async fn post_move(app: axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
    send(
        app,
        json_request("POST", MOVE_PATH, Some(body), Some(token)),
    )
    .await
}

// ===========================================================================
//  三条路径的端到端
// ===========================================================================

/// 无工艺链零件的 send → receive 全链路走通，`current_process_step_id` 两端都落 NULL。
///
/// 无链零件发外协曾被 `20706 BIZ_PROCESS_CHAIN_REQUIRED` 拦在 send 之前，而生产库里
/// 绝大多数零件没有链 ⇒ 绝大多数货根本发不出去（候选卡恒空也是同一个根因）。
/// `current_process_step_id` 早已被官方降级为「可选的显示用定位信息」。
#[tokio::test]
async fn move_send_and_receive_without_process_chain_succeeds() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "NoChain", "N").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "NoChainCo").await;
    let proc_id = seed_outsource_process(&pool, "PNC-SND", "noc_send", true).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "NOC-SND", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    // 前提断言：part 确实没有链
    let chain: Option<i64> =
        sqlx::query_scalar("SELECT process_chain_id FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("read process_chain_id");
    assert!(chain.is_none(), "本用例前提是 part 无工艺链");

    // ---- send ----
    let (s, env) = post_move(
        app.clone(),
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "无链零件也必须能发外协: {env}");
    assert_eq!(env["data"]["to_kind"], "OUTSOURCE_COMPANY", "{env}");
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
    let (recv_shelf, next_proc) = setup_receive_side(&pool, None, "NOC-REC", "PNC-REC").await;
    let version = batch_version(&pool, bid).await;
    let (s, env) = post_move(
        app.clone(),
        &token,
        receive_body(bid, version, company_id, recv_shelf, Some(next_proc)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "无链零件也必须能收回: {env}");
    assert_eq!(env["data"]["to_kind"], "PRODUCTION_SHELF", "{env}");
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

/// `IN_PROCESS` 源（`location='PRODUCTION_SHELF'`）的无链零件发送**成功**。
///
/// 端到端实测曾发现这里 100% 被拒：状态机白名单只有 `PENDING → OUTSOURCE`，没有
/// `IN_PROCESS → OUTSOURCE`，而 move 端点拿 batch 源状态去 `ensure_transition` ⇒ 一律
/// 20103。影响面是全量（候选卡按 `current_process_id` 出行，出行批次几乎全是
/// `IN_PROCESS` 源）。
#[tokio::test]
async fn move_send_from_in_process_shelf_batch_succeeds() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "InProcSnd", "Z").await;
    let part_id = insert_part(&pool, customer_id, "IN_PROCESS").await;
    let company_id = insert_outsource_company(&pool, "InProcSndCo").await;
    // 免审批工序：DIRECT 合法
    let proc_id = seed_outsource_process(&pool, "ZIPS", "inproc_send", false).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "ZIPS-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;

    let (s, env) = post_move(
        app.clone(),
        &token,
        send_body_direct(bid, 0, shelf_id, company_id),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "IN_PROCESS 源在生产架上必须能发外协: {env}"
    );
    assert_eq!(env["data"]["to_kind"], "OUTSOURCE_COMPANY", "{env}");

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
    assert!(step.is_none(), "无链时 step 落 NULL");

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

/// `PENDING` 源（但在生产架上）同样可发 —— 状态机白名单的 `PENDING → OUTSOURCE` 那条边。
///
/// 这条边的可达性比上一条窄（三合一后 `from.kind` 恒为 `PRODUCTION_SHELF`，未上架的
/// PENDING 批次进不来），但它是真实数据形态的一部分（`place-on-shelf` 之前的 PENDING
/// 批次若已写 holder）。
#[tokio::test]
async fn move_send_from_pending_batch_on_shelf_succeeds() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "PendSnd", "W").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "PendSndCo").await;
    let proc_id = seed_outsource_process(&pool, "PENDSND", "pend", true).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PEND-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "PENDING",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "PENDING 源（在架上）必须能发外协: {env}");
    let (status,): (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "OUTSOURCE");
}

/// `IN_PROCESS` 源但 `location != 'PRODUCTION_SHELF'`（例如在工人手上）仍**拒收**，20103。
///
/// 与「`IN_PROCESS` 源能发」成对：状态机补 `IN_PROCESS → OUTSOURCE` 后，`IN_PROCESS`
/// 源的不变式就只剩这道 location 守卫。货还在工人手上时不该被发到外协。
#[tokio::test]
async fn move_send_rejects_in_process_off_production_shelf() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "InProcOff", "Y").await;
    let part_id = insert_part(&pool, customer_id, "IN_PROCESS").await;
    let company_id = insert_outsource_company(&pool, "InProcOffCo").await;
    let proc_id = seed_outsource_process(&pool, "YIPO", "inproc_off", false).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "YIPO-SH", "PRODUCTION").await;
    // location = WORKER：货在工人手上，不在生产架
    let bid =
        insert_batch_with_process(&pool, part_id, "IN_PROCESS", Some("WORKER"), proc_id, None)
            .await;

    let (s, env) = post_move(app, &token, send_body_direct(bid, 0, shelf_id, company_id)).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "不在生产架上的 IN_PROCESS 批次必须拒: {env}"
    );
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20103,
        "命中 location 不变式守卫（BIZ_INVALID_TRANSITION）: {env}"
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

/// 发送方向：shipment 建开口单 + 报价事件写 `SENT` + part 事件写
/// `SENT_TO_OUTSOURCE`（审计字面量与 WS 事件名是两件事）。
#[tokio::test]
async fn move_send_inserts_shipment_out_sourcing() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Snd", "S").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "SendCo").await;
    let proc_id = seed_outsource_process(&pool, "PSND", "psend", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PSND-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "发送: {env}");

    // 出参的 shipment_id 必须与库里那张开口单逐字一致（前端把它挂到卡片上）
    let shipment_id = env["data"]["shipment_id"]
        .as_str()
        .expect("shipment_id 必须是字符串")
        .to_string();
    let row: (i64, String, i64, String, i32) = sqlx::query_as(
        "SELECT id, status, part_id, unit_price::text, quantity \
         FROM t_outsource_shipment WHERE batch_id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .expect("shipment row");
    assert_eq!(row.0.to_string(), shipment_id, "{env}");
    assert_eq!(row.1, "OUTSOURCING");
    assert_eq!(row.2, part_id);
    assert_eq!(row.3, "12.50");
    assert_eq!(
        row.4, 5,
        "move 是整批语义：shipment.quantity = 批次当前余量"
    );

    // 报价事件 SENT
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_outsource_quote_event \
         WHERE quote_id = $1 AND event_type = 'SENT'",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    // part 事件审计字面量逐字不变
    let ev: (i64, String) = sqlx::query_as(
        "SELECT batch_id, event_type FROM t_part_event \
         WHERE part_id = $1 AND event_type = 'SENT_TO_OUTSOURCE'",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .expect("part_event SENT_TO_OUTSOURCE");
    assert_eq!(ev, (bid, "SENT_TO_OUTSOURCE".to_string()));
}

/// 批次已经有**开口** shipment 时再发送 → 21502
/// （`BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION`，`uq_t_outsource_shipment_open_batch`
/// 是 partial unique，一个批次最多一张开口单）。
///
/// 造这形态要用 raw SQL：走完整流程发出去的批次已经在公司手上，会先被状态机
/// （`OUTSOURCE → OUTSOURCE` 不允许，20103）与 `from` 守卫拦掉，压根到不了 shipment
/// 这一步。库里的这批行来自旧的部分收发语义（源批次余量继续持有开口单）与手工改库。
#[tokio::test]
async fn move_send_rejects_when_batch_already_has_open_shipment() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Dup", "D").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "DupCo").await;
    let proc_id = seed_outsource_process(&pool, "PDUP", "dup", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PDUP-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    // 预置一张开口 shipment（批次仍在架上 —— 这正是要走这条守卫的形态）
    let now = now_naive();
    let ship_id: i64 = next_id();
    sqlx::query(
        "INSERT INTO t_outsource_shipment \
         (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
          quantity, unit_price, status, sent_at, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, 5, 12.50, 'OUTSOURCING', $7, 0, $7, $7)",
    )
    .bind(ship_id)
    .bind(quote_id)
    .bind(part_id)
    .bind(bid)
    .bind(company_id)
    .bind(proc_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert open shipment");

    let (s, env) = post_move(
        app.clone(),
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "已有开口 shipment 不得再发: {env}"
    );
    assert_eq!(env["code"].as_i64().unwrap(), 21502, "{env}");

    // 批次未被改动，也没有第二张单
    let (status, version): (String, i32) =
        sqlx::query_as("SELECT status, version FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "IN_PROCESS", "被拒请求不得改批次状态: {env}");
    assert_eq!(version, 0, "被拒请求不得推 version: {env}");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "被拒请求不得留下第二张 shipment: {env}");

    // 开口单关闭后同一批次可以再发（partial unique 只约束开口单），两张单并存
    sqlx::query(
        "UPDATE t_outsource_shipment SET status = 'RECEIVED', received_at = now() \
         WHERE batch_id = $1",
    )
    .bind(bid)
    .execute(&pool)
    .await
    .unwrap();
    let (s, env2) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "开口单已关闭时可以再发: {env2}");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
}

/// 已经发出去的批次（`OUTSOURCE` + 在公司名下）再从生产架发一次 → 状态机先拒（20103）。
///
/// 守卫顺序的实证：状态机（守卫 ⑤）排在 `from` 锚点（守卫 ⑥）之前，所以这种「两个
/// 事实同时不成立」的请求拿到的是 20103 而不是 20122 —— 归因上「这批货已经出去了」比
/// 「你指的起点不对」更准。
#[tokio::test]
async fn move_send_twice_rejected_by_state_machine_first() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Twice", "E").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "TwiceCo").await;
    let proc_id = seed_outsource_process(&pool, "PTWICE", "twice", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PTWICE-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;

    let (s, env1) = post_move(
        app.clone(),
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "首次发送: {env1}");

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 1, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "已发出去的批次不能再发一次: {env}"
    );
    assert_eq!(env["code"].as_i64().unwrap(), 20103, "{env}");
}

/// DIRECT 免审批直发：无可用报价时自动建 `price=0` 的 APPROVED 占位报价
/// （`is_direct=true`），shipment 单价落 0 且请求成功。
#[tokio::test]
async fn move_send_direct_creates_zero_price_placeholder_quote() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Dir", "I").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "DirCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIR", "dir", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PDIR-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;

    let (s, env) = post_move(app, &token, send_body_direct(bid, 0, shelf_id, company_id)).await;
    assert_eq!(s, StatusCode::OK, "direct 直发: {env}");

    let quote: (String, String, Option<String>, bool) = sqlx::query_as(
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
        quote.2.as_deref().unwrap_or_default().contains("DIRECT"),
        "占位报价 note 应写明来源便于对账识别，实际：{:?}",
        quote.2
    );
    assert!(
        quote.3,
        "占位报价必须标 is_direct=true 以避开审批报价唯一索引"
    );

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

/// migration 008：同一个 `(part_id, outsource_company_id, process_id)` tuple 连发两次
/// DIRECT，只允许存在 **1 条** `is_direct = true` 的 0 元占位报价，两张 shipment 共用
/// 同一个 `quote_id`。
///
/// 锁的是两件独立的事：
/// 1. **串行幂等**（`find_approved_quote_id` 复用路径）——第二次不新建占位报价；
/// 2. **约束真的存在**（`uq_t_outsource_quote_direct_part_company_process`）——绕过
///    service 直接再插一条同 tuple 的占位报价必须被拒（0 行），否则并发窗口（双击 /
///    超时重试 / 两个批次同 tuple 直发）仍会各留一条等价记录。
#[tokio::test]
async fn move_send_direct_same_tuple_keeps_single_placeholder_quote() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "DirDup", "C").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "DirDupCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIRDUP", "dirdup", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PDIRDUP-SH", "PRODUCTION").await;
    // 同一 part 的两个批次（batch_no 必须不同 —— `uq_t_part_batch_part_no`）
    let b1 = insert_nth_batch(&pool, part_id, 1, "IN_PROCESS", Some("PRODUCTION_SHELF"), 5).await;
    let b2 = insert_nth_batch(&pool, part_id, 2, "IN_PROCESS", Some("PRODUCTION_SHELF"), 5).await;
    for bid in [b1, b2] {
        sqlx::query(
            "UPDATE t_part_batch SET current_process_id = $2, current_holder_id = $3 WHERE id = $1",
        )
        .bind(bid)
        .bind(proc_id)
        .bind(shelf_id)
        .execute(&pool)
        .await
        .expect("bind batch to process + holder");
        let (s, env) = post_move(
            app.clone(),
            &token,
            send_body_direct(bid, 0, shelf_id, company_id),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "DIRECT 直发 batch {bid}: {env}");
    }

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
    let dup_id: i64 = next_id();
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

/// DIRECT 命中活跃 APPROVED 报价时**复用**它（不新建占位），shipment 单价等于该报价单价。
#[tokio::test]
async fn move_send_direct_reuses_active_approved_quote() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "DirR", "J").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "DirReuseCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIRREUSE", "dirreuse", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PDIRREUSE-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let quote_id = insert_quote_with_price(&pool, part_id, company_id, proc_id, "8.80").await;

    let (s, env) = post_move(app, &token, send_body_direct(bid, 0, shelf_id, company_id)).await;
    assert_eq!(s, StatusCode::OK, "direct 复用: {env}");

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

/// `direct=true` 与 `quote_id` 互斥（两种价来源不能同时给）。
#[tokio::test]
async fn move_send_direct_with_quote_id_rejected() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "DirX", "K").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "DirXCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIRX", "dirx", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PDIRX-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let mut body = send_body_with_quote(bid, 0, shelf_id, company_id, quote_id);
    body["direct"] = json!(true);
    let (s, env) = post_move(app, &token, body).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "direct+quote_id: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "IN_PROCESS");
}

/// `requires_approval = true` 的工序 + `direct = true` → 400 / 20104，且**任何一行都不许
/// 被改**。
///
/// 守卫的必要性：改之前 `requires_approval` 只在读侧（看板候选卡判定 SQL）生效，写侧
/// 零校验 ⇒ 绕过 UI 直接调本端点传 `direct=true` 就能对「先审批再发」这道业务规则该走
/// 报价的工序直发。写侧守了之后读侧/写侧才闭环。
#[tokio::test]
async fn move_send_direct_rejected_when_process_requires_approval() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "ApReq", "H").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "ApReqCo").await;
    let proc_id = seed_outsource_process(&pool, "PAPR", "apreq", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PAPR-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;

    let (s, env) = post_move(app, &token, send_body_direct(bid, 0, shelf_id, company_id)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "需审批工序不许直发: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104, "{env}");

    let (status, location, holder, version): (String, Option<String>, Option<i64>, i32) =
        sqlx::query_as(
            "SELECT status, location, current_holder_id, version \
             FROM t_part_batch WHERE id = $1",
        )
        .bind(bid)
        .fetch_one(&pool)
        .await
        .expect("read batch after rejected direct send");
    assert_eq!(status, "IN_PROCESS", "被拒请求不得改批次状态: {env}");
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(holder, Some(shelf_id), "holder 不得被清: {env}");
    assert_eq!(version, 0, "被拒请求不得推 version: {env}");

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

/// 同一道 `requires_approval = true` 的工序走 APPROVAL（传 `quote_id`）**放行** ——
/// 守卫只拦 `direct=true`，不能误伤正常审批流。
#[tokio::test]
async fn move_send_approval_allowed_when_process_requires_approval() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "ApOk", "O").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "ApOkCo").await;
    let proc_id = seed_outsource_process(&pool, "PAPOK", "apok", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PAPOK-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let quote_id = insert_quote_with_price(&pool, part_id, company_id, proc_id, "33.30").await;

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "需审批工序走 APPROVAL 必须放行: {env}");
    let (unit_price,): (String,) =
        sqlx::query_as("SELECT unit_price::text FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("shipment row");
    assert_eq!(unit_price, "33.30", "APPROVAL 发货必须用审批价");
}

/// 需审批工序 + `quote_id` 直指一条 `is_direct=true` 的 APPROVED 占位报价 → 400 / 21307。
///
/// 「需审批的工序只能凭真审批价发货」有两条入口，`direct=true` 由 `requires_approval`
/// 守卫拦，`quote_id` 这条靠本守卫：占位报价是 `status='APPROVED' / is_direct=true /
/// price=0` 的自动行，只看状态与 (part, company, process) 三元组时它与真审批报价无法
/// 区分。库里的占位报价来自守卫上线前的历史数据、或 `PATCH /prod/processes/{id}` 把
/// `requires_approval` 由 false 翻成 true，所以不能靠清数据消除。
#[tokio::test]
async fn move_send_rejects_direct_placeholder_quote_as_approval_price() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "PhDir", "D").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "PhDirCo").await;
    let proc_id = seed_outsource_process(&pool, "PDIR", "pdir", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PDIR-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let quote_id = insert_direct_placeholder_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
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

    let (status, version): (String, i32) =
        sqlx::query_as("SELECT status, version FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("read batch after rejected placeholder-quote send");
    assert_eq!(status, "IN_PROCESS", "被拒请求不得改批次状态: {env}");
    assert_eq!(version, 0, "被拒请求不得推 version: {env}");

    // 占位报价行原样保留（守卫只拒请求，不改数据）
    let (still_direct, price): (bool, String) =
        sqlx::query_as("SELECT is_direct, price::text FROM t_outsource_quote WHERE id = $1")
            .bind(quote_id)
            .fetch_one(&pool)
            .await
            .expect("placeholder quote row");
    assert!(still_direct, "被拒请求不得改占位报价: {env}");
    assert_eq!(price, "0.00", "{env}");
}

/// 回归：非需审批工序（`requires_approval=false`）走 DIRECT + 复用 `is_direct=true`
/// 占位报价**仍放行** —— 新守卫只作用于 `quote_id` 路径。
#[tokio::test]
async fn move_send_direct_still_reuses_placeholder_quote_when_approval_not_required() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "NoAp", "Y").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "NoApCo").await;
    let proc_id = seed_outsource_process(&pool, "PNOAP", "noap", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PNOAP-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let quote_id = insert_direct_placeholder_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = post_move(app, &token, send_body_direct(bid, 0, shelf_id, company_id)).await;
    assert_eq!(s, StatusCode::OK, "免审批工序走 DIRECT 必须放行: {env}");
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

/// 既不给 `direct` 也不给 `quote_id` → 400。
///
/// 守卫的必要性：没有价来源时 shipment 的 `unit_price` 只能落 0，而对账页看到「单价 0」
/// 无从判断是漏填还是 DIRECT 免审批直发。
#[tokio::test]
async fn move_send_without_price_source_rejected() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "NoP", "N").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "NoPCo").await;
    let proc_id = seed_outsource_process(&pool, "PNOP", "nop", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PNOP-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;

    let (s, env) = post_move(app, &token, send_body(bid, 0, shelf_id, company_id)).await;
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

/// process 类别不是 `OUTSOURCE` → 400。
///
/// 守卫前把货派给内部工序也照样成功，批次随后被标成 OUTSOURCE + 挂外协公司。
#[tokio::test]
async fn move_send_rejects_non_outsource_process_category() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Cat", "G").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "CatCo").await;
    let proc_id = seed_process(&pool, "PASM", "asm", "INHOUSE", false).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PASM-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "非外协工序: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104);
}

/// 公司未映射该工序 → 400（公司在册 ≠ 有该工序能力）。
#[tokio::test]
async fn move_send_rejects_company_without_process_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Map", "M").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "MapCo").await;
    let proc_id = seed_outsource_process(&pool, "PMAP", "map", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    // 刻意不调 map_company_process
    let shelf_id = insert_shelf(&pool, "PMAP-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "公司未映射工序: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20104);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "IN_PROCESS", "被拒请求不得改批次状态");
}

/// 公司不存在 → 21201；公司已停用 → 21205（两条是「在册但不可用」的两种成因）。
#[tokio::test]
async fn move_send_rejects_missing_or_inactive_company() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "CoState", "A").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let proc_id = seed_outsource_process(&pool, "PCOST", "cstate", true).await;
    let shelf_id = insert_shelf(&pool, "PCOST-SH", "PRODUCTION").await;
    let quote_id = {
        let company_id = insert_outsource_company(&pool, "CoStateCo").await;
        map_company_process(&pool, company_id, proc_id).await;
        insert_approved_quote(&pool, part_id, company_id, proc_id).await
    };
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let company_id: i64 =
        sqlx::query_scalar("SELECT outsource_company_id FROM t_outsource_quote WHERE id = $1")
            .bind(quote_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    // ① 不存在的公司 → 21201
    let missing = company_id + 1;
    let (s, env) = post_move(
        app.clone(),
        &token,
        send_body_with_quote(bid, 0, shelf_id, missing, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "公司不存在: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21201, "{env}");

    // ② 停用公司 → 21205
    sqlx::query("UPDATE t_outsource_company SET is_active = false WHERE id = $1")
        .bind(company_id)
        .execute(&pool)
        .await
        .unwrap();
    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "公司停用: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21205, "{env}");
}

/// part **有**工艺链、但链内没有这道外协工序的 step ⇒ 仍以
/// `20702 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`（HTTP 404）拒收。
///
/// 这是「链可选」放松的**边界用例**，与「无链零件收发闭环」构成对照：两条放行/拒收的
/// 判据只有 `process_chain_id` 是否为 NULL 这一个区别。跟着「没链就放行」把 20702 一起
/// 吞掉的后果是：批次带着一个链内不存在的工序静默入池，之后每一步的 step 定位全部漂移。
#[tokio::test]
async fn move_send_rejects_process_missing_from_existing_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Step", "T").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "StepCo").await;
    let proc_id = seed_outsource_process(&pool, "PSTP", "step", true).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PSTP-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    let quote_id = insert_approved_quote(&pool, part_id, company_id, proc_id).await;
    // 链里只登记**另一道**工序（INHOUSE），外协工序 proc_id 刻意不入链
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let other = seed_process(&pool, "PSTP-OTH", "oth", "INHOUSE", false).await;
    create_step(&pool, chain_id, other, 1).await;

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, quote_id),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "链内无该工序必须拒: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20702);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "IN_PROCESS", "被拒请求不得改批次状态");
}

/// `quote_id` 指向 DRAFT 报价 → 21307（与 `is_direct` 占位价同码：两者都是「这不是可用的
/// 审批价来源」）。
#[tokio::test]
async fn move_send_quote_not_approved_returns_21307() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "Qd", "Q").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "QdCo").await;
    let proc_id = seed_outsource_process(&pool, "PQ", "pq", true).await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, proc_id, 1).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "PQ-SH", "PRODUCTION").await;
    let bid = insert_batch_with_process(
        &pool,
        part_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf_id),
    )
    .await;
    // raw SQL 插一个 DRAFT quote（不走 service 校验）
    let qid: i64 = next_id();
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

    let (s, env) = post_move(
        app,
        &token,
        send_body_with_quote(bid, 0, shelf_id, company_id, qid),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "draft q: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21307);
}

/// 回收方向：shipment 标 `RECEIVED` + 写 `received_at` + 报价事件 `RECEIVED` + part
/// 事件 `RECEIVED_FROM_OUTSOURCE`，批次工序推进到 `next_process_id`。
#[tokio::test]
async fn move_receive_marks_shipment_received() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, part_id, company_id, quote_id, shelf_id, next_proc, _) =
        setup_inflight(&pool, "R", "R", "PR", "REC-1", "REC-PROC", true).await;

    let (s, env) = post_move(
        app.clone(),
        &token,
        receive_body(bid, 0, company_id, shelf_id, Some(next_proc)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "回收: {env}");
    assert_eq!(env["data"]["to_kind"], "PRODUCTION_SHELF", "{env}");
    assert_eq!(
        env["data"]["new_location"], "PRODUCTION_SHELF",
        "new_location 恒等于 to.kind: {env}"
    );
    assert_eq!(
        env["data"]["new_process_id"],
        next_proc.to_string(),
        "出参的 new_process_id 是字符串: {env}"
    );

    let (status, received_at): (String, Option<chrono::NaiveDateTime>) =
        sqlx::query_as("SELECT status, received_at FROM t_outsource_shipment WHERE batch_id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "RECEIVED");
    assert!(received_at.is_some(), "整批回收必须写 received_at");
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_outsource_quote_event \
         WHERE quote_id = $1 AND event_type = 'RECEIVED'",
    )
    .bind(quote_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let ev: (i64, String) = sqlx::query_as(
        "SELECT batch_id, event_type FROM t_part_event \
         WHERE part_id = $1 AND event_type = 'RECEIVED_FROM_OUTSOURCE'",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .expect("part_event RECEIVED_FROM_OUTSOURCE");
    assert_eq!(ev.0, bid);
}

/// 回收时**省略** `to.next_process_id` ⇒ 后端按工序链推导下一道工序（看板在途卡的
/// `chain_resolvable = true` 与此同源：`repo/sql.rs::NEXT_PROCESS_LATERAL_SQL`）。
#[tokio::test]
async fn move_receive_derives_next_process_from_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _part_id, company_id, _quote_id, shelf_id, next_proc, _) =
        setup_inflight(&pool, "Derive", "V", "PDER", "REC-D", "REC-PROC-D", true).await;

    let (s, env) = post_move(
        app,
        &token,
        receive_body(bid, 0, company_id, shelf_id, None),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "工序链可推导时不得要求手填: {env}");
    assert_eq!(
        env["data"]["new_process_id"],
        next_proc.to_string(),
        "推导出的下一道工序必须与看板在途卡的 receive_next_process_id 一致: {env}"
    );
    let (status, cur_proc, holder): (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT status, current_process_id, current_holder_id FROM t_part_batch WHERE id = $1",
    )
    .bind(bid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(cur_proc, Some(next_proc));
    assert_eq!(holder, Some(shelf_id));
}

/// 回收时省略 `to.next_process_id` 但零件**没有工序链** ⇒ 20706，且批次一个字节都没动。
///
/// 与上一条成对：两条的差别只有「有没有锚链」。文案也要分开 —— 无链时用户该去
/// 制定工序链，有链但推不出时（指针漂移 / 已是最后一步）该手填下一道工序。
#[tokio::test]
async fn move_receive_without_chain_returns_20706() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _part_id, company_id, _quote_id, shelf_id, _next_proc, _) = setup_inflight(
        &pool,
        "NoChainRecv",
        "A",
        "PNCR",
        "REC-NC",
        "REC-PROC-NC",
        false,
    )
    .await;

    let (s, env) = post_move(
        app,
        &token,
        receive_body(bid, 0, company_id, shelf_id, None),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "无链推不出: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20706, "{env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("工序链"),
        "文案必须指向「制定工序链 / 手填下一道工序」: {env}"
    );

    let (status, location, version): (String, Option<String>, i32) =
        sqlx::query_as("SELECT status, location, version FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "OUTSOURCE", "被拒请求不得改批次状态");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(version, 0, "被拒请求不得推 version");
}

/// 非发送方向带了 `quote_id` / `direct` → 20104（这两字段只服务发送方向的价来源）。
#[tokio::test]
async fn move_receive_rejects_price_source_fields() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _part_id, company_id, quote_id, shelf_id, next_proc, _) =
        setup_inflight(&pool, "RejectPS", "A", "PRJPS", "REC-J", "REC-PROC-J", true).await;

    for (field, value) in [
        ("quote_id", json!(quote_id.to_string())),
        ("direct", json!(true)),
    ] {
        let mut body = receive_body(bid, 0, company_id, shelf_id, Some(next_proc));
        body[field] = value;
        let (s, env) = post_move(app.clone(), &token, body).await;
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "{field} 在回收方向必须拒: {env}"
        );
        assert_eq!(env["code"].as_i64().unwrap(), 20104, "{field}: {env}");
    }
}

/// 回收直送品检：落 `INSPECTION` + 品检架，`current_process_id` / step 按出池不变式清
/// 2026-10-10：`OUTSOURCE_COMPANY → INSPECTION_SHELF` 方向**整条下线**。
///
/// `OutsourceLocation::InspectionShelf` 变体随该方向一并删除，于是发这个 kind 的
/// 请求在**反序列化阶段**就被拒：axum 的 `Json` 提取器返回 422 **纯文本**
/// （`unknown variant \`INSPECTION_SHELF\``），**不进 `R<T>` 信封**。
///
/// 断言的是这个「硬切无 alias」形态，而不是某个业务错误码 —— 该方向连 service 都
/// 进不去。替代路径：先收进生产架（`to.kind = PRODUCTION_SHELF`），再走常规送检
/// 链路，品检架同样由服务端自动选。
#[tokio::test]
async fn move_receive_to_inspection_direction_is_gone() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _part_id, company_id, _quote_id, _shelf, _next_proc, _) =
        setup_inflight(&pool, "Insp", "Z", "PINSP", "REC-I", "REC-PROC-I", true).await;
    let insp_shelf = insert_shelf(&pool, "INS-1", "INSPECTION").await;

    // 422 纯文本不进 `R<T>` 信封，故用 `send_raw` 而不是 `post_move`（后者会 panic）
    let (s, raw) = send_raw(
        app.clone(),
        json_request(
            "POST",
            MOVE_PATH,
            Some(json!({
                "batch_id": bid.to_string(),
                "version": 0,
                "from": { "kind": "OUTSOURCE_COMPANY", "company_id": company_id.to_string() },
                "to": { "kind": "INSPECTION_SHELF", "shelf_id": insp_shelf.to_string() },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "已下线的 kind 必须在反序列化阶段被拒: {raw}"
    );
    assert!(
        raw.contains("unknown variant") && raw.contains("INSPECTION_SHELF"),
        "响应应点名被删掉的 kind: {raw}"
    );
    // 批次不受影响（错误发生在 service 之前）
    let status: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "OUTSOURCE", "被拒的请求不得改动批次");
}

// ===========================================================================
//  三合一新增的请求形状守卫
// ===========================================================================

/// 同 kind 移动（`from.kind == to.kind`）→ 40001，且**早于查批次**（用一个根本不存在的
/// batch_id 也能拿到 40001 而不是 20109）。
///
/// 顺序本身就是契约：同 kind 是请求形状错误，与哪一批货无关。先查库会让「批次不存在」
/// 掩盖「你把同一个位置同时当成起点和终点」。
#[tokio::test]
async fn move_same_kind_returns_40001_before_batch_lookup() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let _ = pool;
    let ghost = 9_000_000_000_000_099_999i64;
    let body = json!({
        "batch_id": ghost.to_string(),
        "version": 0,
        "from": { "kind": "OUTSOURCE_COMPANY", "company_id": "123" },
        "to": { "kind": "OUTSOURCE_COMPANY", "company_id": "456" },
    });
    let (s, env) = post_move(app, &token, body).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "同 kind: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40001, "{env}");
}

/// 批次不存在 / 已软删 → 20109。
#[tokio::test]
async fn move_unknown_batch_returns_20109() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let body = json!({
        "batch_id": "9000000000000099999",
        "version": 0,
        "from": { "kind": "OUTSOURCE_COMPANY", "company_id": "123" },
        "to": { "kind": "PRODUCTION_SHELF", "shelf_id": "456" },
    });
    let (s, env) = post_move(app, &token, body).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "批次不存在: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20109, "{env}");
}

/// `from` 的 `kind` 与批次真实 location 不符 → 20122。
#[tokio::test]
async fn move_from_kind_mismatch_is_rejected() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _part_id, company_id, quote_id, shelf_id, next_proc, _) =
        setup_inflight(&pool, "KindMM", "K", "PKIND", "REC-K", "REC-PROC-K", true).await;

    // 批次在 OUTSOURCE_COMPANY，from 却报生产架 —— 守卫 ⑤（状态机）先于 ⑥（from 锚点）
    // 命中：`OUTSOURCE → OUTSOURCE` 不是合法边，所以这一形态**不会**走到 20122。
    // 2026-10-10 只剩两个 kind 之后，「from 报生产架 + to 报外协公司」是这个唯一的
    // 跨 kind 组合，故 20122 的可达形态收窄为「from 报外协公司但 company_id 不符」。
    let body = json!({
        "batch_id": bid.to_string(),
        "version": 0,
        "from": { "kind": "PRODUCTION_SHELF" },
        "to": { "kind": "OUTSOURCE_COMPANY", "company_id": company_id.to_string() },
    });
    let (s, env) = post_move(app.clone(), &token, body).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "OUTSOURCE → OUTSOURCE 应先被状态机拒（不是 20122）: {env}"
    );
    assert_eq!(env["code"].as_i64().unwrap(), 20103, "{env}");

    // from 报外协公司但 company_id 对不上：2026-10-10 起 `to.kind` 只剩
    // PRODUCTION_SHELF / OUTSOURCE_COMPANY 两值（品检架方向已下线），故这一段改用
    // 「外协公司侧 holder 不符」来触发同一条 20122 守卫。
    let body = json!({
        "batch_id": bid.to_string(),
        "version": 0,
        "from": { "kind": "OUTSOURCE_COMPANY", "company_id": (company_id + 1).to_string() },
        "to": { "kind": "PRODUCTION_SHELF", "next_process_id": next_proc.to_string() },
    });
    let (s, env) = post_move(app, &token, body).await;
    assert_eq!(s, StatusCode::CONFLICT, "外协公司 id 不符: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20122, "{env}");
    let _ = (quote_id, shelf_id);
}

/// `from` 的 holder id 与批次真实 `current_holder_id` 不符 → 20122。
///
/// 这是三合一最核心的新守卫：看板卡片可能过期（另一台机器已经把货挪走），拖拽时必须
/// 被拒而不是把货从错误的公司挪走。
#[tokio::test]
async fn move_from_holder_mismatch_returns_20122() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _part_id, company_id, _quote_id, shelf_id, next_proc, _) =
        setup_inflight(&pool, "HolderMM", "H", "PHOLD", "REC-H", "REC-PROC-H", true).await;

    let (s, env) = post_move(
        app.clone(),
        &token,
        receive_body(bid, 0, company_id + 1, shelf_id, Some(next_proc)),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "from.company_id 与真实 holder 不符: {env}"
    );
    assert_eq!(env["code"].as_i64().unwrap(), 20122, "{env}");

    // 批次未被改动
    let (status, location): (String, Option<String>) =
        sqlx::query_as("SELECT status, location FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "OUTSOURCE");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
}

/// `version` 过期 → 40901（OCC 锚来自候选卡 / 在途卡，读到什么就回传什么）。
#[tokio::test]
async fn move_stale_version_returns_40901() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, _part_id, company_id, _quote_id, shelf_id, next_proc, _) =
        setup_inflight(&pool, "Stale", "T", "PSTALE", "REC-S", "REC-PROC-S", true).await;

    let (s, env) = post_move(
        app.clone(),
        &token,
        receive_body(bid, 99, company_id, shelf_id, Some(next_proc)),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "过期 version: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40901, "{env}");

    // 顺带钉住「改过 version 之后正确的新值仍能过」
    sqlx::query("UPDATE t_part_batch SET version = 5 WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .unwrap();
    let (s, env) = post_move(
        app,
        &token,
        receive_body(bid, 5, company_id, shelf_id, Some(next_proc)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "刷新后的 version 必须能过: {env}");
}

/// `version` 是必填字段：body 里没有 ⇒ axum 的 `422` + **纯文本**，不进 `R<T>` 信封。
///
/// 用 `send_raw`：`Json` 提取器的反序列化拒绝是纯文本 body，`send` 会在 JSON 解析处
/// panic。前端错误处理必须按 HTTP 状态码分支，不能假设响应必有 `code` 字段。
#[tokio::test]
async fn move_requires_version_field_returns_422_plain_text() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, body) = send_raw(
        app,
        json_request(
            "POST",
            MOVE_PATH,
            Some(json!({
                "batch_id": "1",
                "from": { "kind": "OUTSOURCE_COMPANY", "company_id": "1" },
                "to": { "kind": "PRODUCTION_SHELF", "shelf_id": "1" },
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "缺 version 必须是 422（不是业务信封）: {body}"
    );
    assert!(
        body.contains("version"),
        "纯文本 body 必须点名缺失字段 version: {body}"
    );
    // 纯文本 ⇒ 不是 JSON 信封（前端按 `code` 解析会拿到 null）
    assert!(
        !body.trim_start().starts_with('{'),
        "422 响应不是 R<T> 信封: {body}"
    );
}

/// 回收方向的源状态**显式**白名单：批次还在生产架上（`IN_PROCESS`）却请求回收到品检架
/// ⇒ `20103`。
///
/// 这条守卫是「显式钉住」而不是顺带的：状态机白名单里 `IN_PROCESS → INSPECTION` 与
/// `PENDING → IN_PROCESS` / `PENDING → INSPECTION` 都是合法边（它们属建档 / 待编程流的
/// 语义），删掉它这些边就会把「从生产架直接回收品检」放进 match 的兜底分支 ⇒ panic
/// 回收方向的**源状态**守卫：批次不在 `OUTSOURCE`（这里是 `IN_PROCESS`）→ 20103。
///
/// 2026-10-10 起 `to` 只剩 `PRODUCTION_SHELF` 一个合法值（直送品检方向下线），
/// 本用例改用「`from` 逐字等于批次真实位置（否则会挂在更早的 20122 上）+ 回收生产」
/// 来触发同一条守卫。文案仍必须点名源状态白名单。
#[tokio::test]
async fn move_recover_requires_outsource_source_status() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "BadRecvKind", "W").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let company_id = insert_outsource_company(&pool, "BadRecvKindCo").await;
    let proc_id = seed_outsource_process(&pool, "XBRK", "brk_proc", false).await;
    map_company_process(&pool, company_id, proc_id).await;
    let shelf_id = insert_shelf(&pool, "BRK-SH", "PRODUCTION").await;
    // 批次**还没上架**（PENDING，**不是**在外协公司）。选 PENDING 而不是 IN_PROCESS
    // 是因为 `IN_PROCESS → IN_PROCESS` 不是状态机白名单里的边，会被 `ensure_transition`
    // 先拒掉、根本走不到「源状态必须是 OUTSOURCE」那条守卫；PENDING → IN_PROCESS 是
    // 合法边，于是能精确地命中目标守卫。
    let bid = insert_batch_with_process(&pool, part_id, "PENDING", None, proc_id, None).await;
    let _ = shelf_id;

    // `from.kind` 是唯一能让本守卫可达的取值（kind 不同才能通过守卫 ②；批次状态
    // 由守卫 ⑤ 判，⑥ 的 from 锚点守卫在它之后，永不执行）
    let (s, env) = post_move(
        app,
        &token,
        json!({
            "batch_id": bid.to_string(),
            "version": 0,
            "from": { "kind": "OUTSOURCE_COMPANY", "company_id": company_id.to_string() },
            "to": { "kind": "PRODUCTION_SHELF", "next_process_id": proc_id.to_string() },
        }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "非 OUTSOURCE 源回收必须拒: {env}"
    );
    assert_eq!(env["code"].as_i64().unwrap(), 20103, "{env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("回收方向的源状态必须是 OUTSOURCE"),
        "文案必须点名源状态白名单: {env}"
    );
}

/// 回收方向省略 `to.next_process_id` 时推不出下一道工序 —— **锚链存在但 step 指针漂移**
/// 这一支文案。
///
/// 与 `move_receive_without_chain_returns_20706`（零件压根没有 `process_chain_id`）是
/// 两条不同的分支：那条走「该零件尚未制定工序链」，本条走「锚链存在但 `cur` 定位不到
/// 当前 step」⇒ 文案要指向「指针漂移 / 补全工序链」，前端据此决定是让用户手填还是先
/// 修链。
#[tokio::test]
async fn move_receive_anchor_chain_without_step_pointer_returns_20706() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, part_id, company_id, _quote_id, shelf_id, _next_proc, _) = setup_inflight(
        &pool,
        "DriftRecv",
        "A",
        "PDRFT",
        "REC-D",
        "REC-PROC-D",
        true,
    )
    .await;

    // 前提断言：锚链在（part 绑了链）
    let chain_id: Option<i64> =
        sqlx::query_scalar("SELECT process_chain_id FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("read part.process_chain_id");
    assert!(chain_id.is_some(), "前提断言：该零件已绑工序链");
    // 制造指针漂移：step 指针被清空 ⇒ 推导片段的 `cur.id = current_process_step_id`
    // 定位不到任何 step，锚链解析失败 ⇒ 推不出下一道工序
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .expect("clear batch step pointer");

    let (s, env) = post_move(
        app,
        &token,
        receive_body(bid, 0, company_id, shelf_id, None),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "有锚链但推不出: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 20706, "{env}");
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("指针漂移") && msg.contains("无法推导下一道工序"),
        "文案必须指向「指针漂移」而不是「尚未制定工序链」: {env}"
    );
    assert!(
        !msg.contains("尚未制定工序链"),
        "锚链存在时不得落到「尚未制定工序链」那一支: {env}"
    );

    let (status, version): (String, i32) =
        sqlx::query_as("SELECT status, version FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "OUTSOURCE", "被拒请求不得改批次状态");
    assert_eq!(version, 0, "被拒请求不得推 version");
}

/// 出参契约：三个雪花 id 都是字符串；`version` 是**读回行的真实值**（不是 `req.version+1`
/// 的算式）；两个方向相关的 `Option` 字段在对应方向有值、在另一方向**键不存在**。
#[tokio::test]
async fn move_result_snowflake_ids_are_strings_and_optional_keys_are_absent() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (bid, part_id, company_id, _quote_id, shelf_id, next_proc, _) =
        setup_inflight(&pool, "Shape", "A", "PSHAPE", "REC-P", "REC-PROC-P", true).await;

    // 预置一个非 0 的 version，让「读回真实值」与「req.version + 1」可区分
    sqlx::query("UPDATE t_part_batch SET version = 7 WHERE id = $1")
        .bind(bid)
        .execute(&pool)
        .await
        .unwrap();

    let (s, env) = post_move(
        app.clone(),
        &token,
        receive_body(bid, 7, company_id, shelf_id, Some(next_proc)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "回收: {env}");
    let data = env["data"].as_object().expect("data 应是 object").clone();

    assert_eq!(data["batch_id"], json!(bid.to_string()), "{env}");
    assert_eq!(data["part_id"], json!(part_id.to_string()), "{env}");
    assert_eq!(data["new_holder_id"], json!(shelf_id.to_string()), "{env}");
    assert_eq!(
        data["new_process_id"],
        json!(next_proc.to_string()),
        "{env}"
    );
    assert_eq!(data["from_kind"], json!("OUTSOURCE_COMPANY"), "{env}");
    assert_eq!(data["to_kind"], json!("PRODUCTION_SHELF"), "{env}");
    assert_eq!(
        data["version"],
        json!(8),
        "version 必须是读回行的真实值: {env}"
    );
    // 回收方向不产生 shipment_id ⇒ 键不存在（不是 null）
    assert!(
        !data.contains_key("shipment_id"),
        "回收方向的 shipment_id 必须整个键不存在: {env}"
    );

    // 发送方向：shipment_id 有值，new_process_id 反向缺席
    let company_id2 = insert_outsource_company(&pool, "ShapeCo2").await;
    let proc_id = seed_outsource_process(&pool, "PSHAPE2", "shape2", false).await;
    map_company_process(&pool, company_id2, proc_id).await;
    let shelf2 = insert_shelf(&pool, "PSHAPE2-SH", "PRODUCTION").await;
    let part2 = insert_part(&pool, 1, "IN_PROCESS").await;
    let bid2 = insert_batch_with_process(
        &pool,
        part2,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        proc_id,
        Some(shelf2),
    )
    .await;
    let (s, env2) = post_move(app, &token, send_body_direct(bid2, 0, shelf2, company_id2)).await;
    assert_eq!(s, StatusCode::OK, "发送: {env2}");
    let data2 = env2["data"].as_object().expect("data 应是 object").clone();
    assert!(
        data2.contains_key("shipment_id"),
        "发送方向必须给 shipment_id: {env2}"
    );
    assert!(data2["shipment_id"].is_string(), "{env2}");
    assert!(
        !data2.contains_key("new_process_id"),
        "发送方向的 new_process_id 必须整个键不存在: {env2}"
    );
    assert_eq!(data2["from_kind"], json!("PRODUCTION_SHELF"), "{env2}");
    assert_eq!(data2["to_kind"], json!("OUTSOURCE_COMPANY"), "{env2}");
    assert_eq!(data2["new_location"], json!("OUTSOURCE_COMPANY"), "{env2}");
    assert_eq!(data2["version"], json!(1), "{env2}");
}

// ===========================================================================
//  shipment 对账（与 move 无关，但同属 shipment 生命周期）
// ===========================================================================

#[tokio::test]
async fn reconcile_update_shipment_unit_price_quantity() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer_id = insert_l1_customer(&pool, "RU", "U").await;
    let part_id = insert_part(&pool, customer_id, "PENDING").await;
    let bid = insert_batch(&pool, part_id, "PENDING", None).await;
    let company_id = insert_outsource_company(&pool, "RecCo").await;
    let proc_id = seed_outsource_process(&pool, "PRU", "pru", true).await;
    let now = now_naive();
    let shipment_id: i64 = {
        let id = next_id();
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
