//! `prod::scan` 外协确认收回端点（`POST /prod/scan/outsource-receive`）集成测试
//!
//! 2026-10-15 新增（第 8 条端点，S3）。覆盖：
//! - happy path：一段事务内「收回落自动选架 + 立刻交到扫码工人」，最终
//!   `IN_PROCESS + WORKER` + holder=工人 + 工序=目标 + `version = 初始 + 2`
//!   （两段 OCC 写各 +1，写后读回）；数量不变；
//! - 下一道解析两条分支：缺省按链推导（`NEXT_PROCESS_LATERAL_SQL` 的
//!   `sort_order = 当前 + 1` 口径）与显式 `next_process_id` 覆盖链推导；
//! - 2026-10-16 新语义（目标工序改由工种映射推导）：链不可推导时**单候选自动推导**
//!   / **零候选 `20104`**；两条既有用例（无链 `40001`、有链推不出 `20706`）的
//!   fixture 相应改成「工种映射 2 道」，钉的是**多候选**分支而非回退旧语义；
//! - 2026-10-16 review 第 1 轮 BLOCKER-1：stage① 传 `StepResolve::Lenient` ⇒
//!   「有链 + 链外工序」落 `NULL` step 而**不是** `20702`，两条形态各有回归
//!   （显式链外 / 链推不出 + 工种单候选链外）；
//! - 判据顺序与错误码全矩阵（`20201` / `20202` / `20206` → `20109` / `20101` →
//!   `40901` → `20103` / `20122` → `40001` / `20104` → `20904` / `20204` → `40300`）；
//! - **关键回归**：容量触顶（`20204`）时**零写入**（批次仍 `OUTSOURCE` /
//!   `OUTSOURCE_COMPANY`、shipment 仍 `OUTSOURCING`、零 part 事件、version 不变）；
//! - 开口 shipment 的关闭（`RECEIVED` + `received_at`）与 quote `RECEIVED` 事件；
//!   无开口 shipment 的 legacy 形态静默跳过；
//! - `t_part_event` 两条事件：`RECEIVED_FROM_OUTSOURCE` + `PICKED_UP` 逐字段。
//!
//! ## fixture 复用
//! 全部从 `super::queue` 借（`bootstrap_as_manager` / `insert_customer_l2` /
//! `seed_process` / `insert_work_type` / `link_work_type_to_process` /
//! `insert_worker` / `insert_worker_held_part` / `append_chain_step` /
//! `part_chain_id` / `insert_shelf` / `link_shelf_to_process` 等），不在本文件
//! 复制同款 fixture 避免漂移。本文件独享的 raw SQL 构造（外协公司 / 开口 shipment /
//! 批次状态直改 / 批次回读）按仓库惯例保留为本地 `async fn`。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::{ProductionFixture, json_request, login_token, pool_snowflake, send};

use super::queue::{
    append_chain_step, bootstrap_as_manager, insert_customer_l2, insert_shelf, insert_work_type,
    insert_worker, insert_worker_held_part, link_shelf_to_process, link_work_type_to_process,
    part_chain_id, seed_process,
};

/// outsource-receive 路径（测试 app 不带 `/api/v2`）。
const RECEIVE_URI: &str = "/prod/scan/outsource-receive";

/// 一个「不存在的批次」id（fixture 区段之外，与任何雪花都物理不相交）。
const MISSING_BATCH_ID: i64 = 9_000_000_000_000_099_999;

// ===========================================================================
//  本 sub-file 独享 helper
// ===========================================================================

/// 打一次 outsource-receive。
async fn receive(app: &axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request("POST", RECEIVE_URI, Some(body), Some(token)),
    )
    .await
}

/// 直插一条外协公司。
async fn insert_outsource_company(pool: &PgPool, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
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

/// 直插一条开口（`OUTSOURCING`）外协 shipment，返回 `(shipment_id, quote_id)`。
async fn insert_open_shipment(
    pool: &PgPool,
    part_id: i64,
    batch_id: i64,
    company_id: i64,
    process_id: i64,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let sid = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let qid = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_shipment (id, quote_id, part_id, batch_id, \
         outsource_company_id, process_id, quantity, unit_price, status, sent_at, \
         created_by, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, 1, 1.00, 'OUTSOURCING', $7, 0, 0)",
    )
    .bind(sid)
    .bind(qid)
    .bind(part_id)
    .bind(batch_id)
    .bind(company_id)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_shipment");
    (sid, qid)
}

/// 直改批次 status / location / holder（构造外协在途形态与两个守卫失败形态）。
async fn force_batch_state(
    pool: &PgPool,
    batch_id: i64,
    status: &str,
    location: Option<&str>,
    holder_id: Option<i64>,
) {
    sqlx::query(
        "UPDATE t_part_batch SET status = $2, location = $3, current_holder_id = $4 \
         WHERE id = $1",
    )
    .bind(batch_id)
    .bind(status)
    .bind(location)
    .bind(holder_id)
    .execute(pool)
    .await
    .expect("force batch state");
}

/// 批次回读：`(status, location, holder, process, step, version, quantity)`。
#[allow(clippy::type_complexity)]
async fn read_batch(
    pool: &PgPool,
    batch_id: i64,
) -> (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i32,
    i32,
) {
    sqlx::query_as(
        "SELECT status, location, current_holder_id, current_process_id, \
         current_process_step_id, version, quantity \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(pool)
    .await
    .expect("read t_part_batch")
}

/// 某 part 的 part 事件条数（零写入断言用）。
async fn count_part_events(pool: &PgPool, part_id: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM t_part_event WHERE part_id = $1")
        .bind(part_id)
        .fetch_one(pool)
        .await
        .expect("count t_part_event")
}

/// `t_part_event` 事件行投影（`outsource_receive_writes_both_part_events` 用）：
/// `(event_type, from_status, to_status, quantity, worker_id, process_id, note)`。
///
/// ⚠️ `quantity` 是 `integer`（`Option<i32>`），其余三个 id 列是 `bigint`；
/// 类型对不上时 `query_as` 会在运行时报 `ColumnDecode`（本用例第一次实跑的形态）。
type PartEventRow = (
    String,
    Option<String>,
    Option<String>,
    Option<i32>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// 一套「可收回的外协批次 + 扫码工人 / 工种 / 生产架 / 外协公司」。
struct ReceiveSetup {
    part_id: i64,
    batch_id: i64,
    worker_id: i64,
    /// 扫码工工牌（`t_worker.badge_code`）。
    badge_code: String,
    company_id: i64,
    work_type_id: i64,
    shelf_id: i64,
    /// 链上「下一道」（工种与货架都映射它；显式流 / 推导流的目标）。
    next_process_id: i64,
    /// 当前工序（链上第一道；`with_chain=false` 时只是批次指针上的工序）。
    current_process_id: i64,
    /// 链内 next 的 step（`with_chain=true` 时 Some；`sort_order = 当前 1 + 1`，
    /// 与写侧 `NEXT_PROCESS_LATERAL_SQL` 的 `= 当前 + 1` 推导口径对齐）。
    next_step_id: Option<i64>,
}

/// 建一套可收回形态：批次先按「工人持有件」造（拿到链 / step / 指针），再强制成
/// `OUTSOURCE + OUTSOURCE_COMPANY + holder=公司`。
///
/// - `with_chain`：工单是否绑链（`false` = 手写工单形态 ⇒ 缺省 `next_process_id` 必拒）；
/// - `map_next`：工种是否映射下一道（`false` = 只映射当前工序，构造 `20104`）。
async fn setup_receive(
    pool: &PgPool,
    serial: &str,
    with_chain: bool,
    map_next: bool,
) -> ReceiveSetup {
    let customer = insert_customer_l2(pool, &format!("{serial}-C")).await;
    let current = seed_process(pool, &format!("{serial}-PC"), "当前工序").await;
    let next = seed_process(pool, &format!("{serial}-PN"), "下一道工序").await;
    let wt = insert_work_type(pool, &format!("{serial}-WT"), "工种", Some(5)).await;
    link_work_type_to_process(pool, wt, if map_next { next } else { current }).await;
    let worker = insert_worker(pool, &format!("{serial}-W"), "扫码工", Some(wt)).await;
    // 货架映射「下一道」（选架候选集只含映射了目标工序的架）
    let shelf = insert_shelf(pool, &format!("{serial}-SH"), "生产架", "PRODUCTION").await;
    link_shelf_to_process(pool, shelf, next).await;
    let company = insert_outsource_company(pool, &format!("{serial}-公司")).await;
    let (part, batch, _step_current) =
        insert_worker_held_part(pool, customer, serial, worker, current, 3, with_chain).await;
    // 链内下一道：`sort_order = 2`（当前 step 是 1）—— 写侧推导按 `= 当前 + 1` 取
    let next_step = if with_chain {
        let chain = part_chain_id(pool, part).await.expect("part 应有链");
        Some(append_chain_step(pool, chain, next, 2).await)
    } else {
        None
    };
    force_batch_state(
        pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;
    ReceiveSetup {
        part_id: part,
        batch_id: batch,
        worker_id: worker,
        badge_code: format!("{serial}-W"),
        company_id: company,
        work_type_id: wt,
        shelf_id: shelf,
        next_process_id: next,
        current_process_id: current,
        next_step_id: next_step,
    }
}

/// 标准请求体（`next_process_id` 传 `Some(next)` 时附字符串形态）。
fn body(st: &ReceiveSetup, version: i32, next: Option<i64>) -> Value {
    let mut v = json!({
        "batch_id": st.batch_id.to_string(),
        "badge_code": st.badge_code,
        "version": version,
    });
    if let Some(p) = next {
        v["next_process_id"] = json!(p.to_string());
    }
    v
}

/// 造一个「工种存在但**一条工序都没映射**」的可收回形态（2026-10-16 零候选用例）。
///
/// 与 [`setup_receive`] 的差别只有一处：扫码工的工种不 `link_work_type_to_process`
/// —— 于是写侧判据 5 的候选集为空，缺省 `next_process_id` 必落 `20104`。
/// 批次形态与 `setup_receive(.., true, true)` 完全一致（无链 / 工种未映射 ⇒ 候选空）。
async fn setup_receive_without_process_mapping(pool: &PgPool, serial: &str) -> ReceiveSetup {
    let customer = insert_customer_l2(pool, &format!("{serial}-C")).await;
    let current = seed_process(pool, &format!("{serial}-PC"), "当前工序").await;
    let next = seed_process(pool, &format!("{serial}-PN"), "下一道工序").await;
    let wt = insert_work_type(pool, &format!("{serial}-WT"), "空工种", Some(5)).await;
    // ⚠️ 刻意**不** link 任何工序
    let worker = insert_worker(pool, &format!("{serial}-W"), "扫码工", Some(wt)).await;
    let shelf = insert_shelf(pool, &format!("{serial}-SH"), "生产架", "PRODUCTION").await;
    link_shelf_to_process(pool, shelf, next).await;
    let company = insert_outsource_company(pool, &format!("{serial}-公司")).await;
    let (part, batch, _step_current) =
        insert_worker_held_part(pool, customer, serial, worker, current, 3, true).await;
    force_batch_state(
        pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;
    ReceiveSetup {
        part_id: part,
        batch_id: batch,
        worker_id: worker,
        badge_code: format!("{serial}-W"),
        company_id: company,
        work_type_id: wt,
        shelf_id: shelf,
        next_process_id: next,
        current_process_id: current,
        next_step_id: None,
    }
}

// ===========================================================================
//  happy path（显式 / 推导）
// ===========================================================================

/// 显式 `next_process_id`：一段事务后批次 `IN_PROCESS + WORKER` + holder=扫码工 +
/// 工序=目标 + `version = 初始 + 2`（两段写各 +1，写后读回）+ 数量不变。
#[tokio::test]
async fn outsource_receive_happy_path_moves_batch_to_worker() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-HAPPY", true, true).await;
    // 扣掉 worker 自己持有目标批次的影响：fixture 已把它改挂公司，worker 手上 0 批。

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::OK, "happy path 应 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["batch_id"], st.batch_id.to_string());
    assert_eq!(env["data"]["part_id"], st.part_id.to_string());
    assert_eq!(env["data"]["worker_id"], st.worker_id.to_string());
    assert_eq!(env["data"]["company_id"], st.company_id.to_string());
    assert_eq!(
        env["data"]["next_process_id"],
        st.next_process_id.to_string()
    );
    assert_eq!(
        env["data"]["version"], 2,
        "两段 OCC 写各 +1，写后读回应为 2: {env}"
    );
    // 键集逐字（雪花全 string，version 裸 number）
    let data = env["data"].as_object().expect("data 是对象");
    let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "batch_id",
            "company_id",
            "next_process_id",
            "part_id",
            "version",
            "worker_id",
        ],
        "OutsourceReceiveOut 字段集变了（同步前端 schema 与 docs/api/scan.md）: {env}"
    );

    let (status, location, holder, process, step, version, quantity) =
        read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(st.worker_id), "holder 必须换成扫码工");
    assert_eq!(process, Some(st.next_process_id), "工序必须推进到目标");
    assert_eq!(step, st.next_step_id, "有链时 step 与工序成对落位");
    assert_eq!(version, 2);
    assert_eq!(quantity, 3, "收回是整批动作，数量不变");
}

/// 不传 `next_process_id`：链 `NEXT`（当前 step 的 `sort_order + 1` 命中下一道）
/// ⇒ 后端按链推导并落链上下一道。
#[tokio::test]
async fn outsource_receive_derives_next_process_from_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-DERIVE", true, true).await;

    let (s, env) = receive(&app, &token, body(&st, 0, None)).await;
    assert_eq!(s, StatusCode::OK, "缺省 next 时应按链推导: {env}");
    assert_eq!(
        env["data"]["next_process_id"],
        st.next_process_id.to_string()
    );

    let (_, _, holder, process, step, version, _) = read_batch(&pool, st.batch_id).await;
    assert_eq!(holder, Some(st.worker_id));
    assert_eq!(process, Some(st.next_process_id), "应落链上推导出的下一道");
    assert_eq!(step, st.next_step_id);
    assert_eq!(version, 2);
}

/// 显式 `next_process_id` **覆盖**链推导：链能推出 proc_next，但 body 指定链内
/// 更后面的 proc_other ⇒ 最终落 proc_other（不是推导值）。
#[tokio::test]
async fn outsource_receive_explicit_next_overrides_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-OVR", true, true).await;
    // 链内第三道：显式覆盖的目标，且工种 / 货架都接纳它
    let other = seed_process(&pool, "OR-OVR-P3", "显式覆盖工序").await;
    link_work_type_to_process(&pool, st.work_type_id, other).await;
    link_shelf_to_process(&pool, st.shelf_id, other).await;
    let chain = part_chain_id(&pool, st.part_id).await.expect("链");
    let other_step = append_chain_step(&pool, chain, other, 3).await;

    let (s, env) = receive(&app, &token, body(&st, 0, Some(other))).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["next_process_id"], other.to_string());
    assert_ne!(
        other, st.next_process_id,
        "前置：显式值与链推导值必须不同，否则本用例证明不了覆盖"
    );

    let (_, _, _, process, step, _, _) = read_batch(&pool, st.batch_id).await;
    assert_eq!(process, Some(other), "显式值必须覆盖链推导");
    assert_eq!(step, Some(other_step));
}

// ===========================================================================
//  下一道解析的失败路径
// ===========================================================================

/// 无链 + 缺省 `next_process_id` + 工种**多道**候选 → `40001`（HTTP 422，零写入）。
///
/// ⚠️ 2026-10-16 调整 fixture：工种映射由 1 道改为 **2 道**。新语义下「无链 + 工种
/// 只有一道」会由服务端自动推导（成功），本用例要钉的是「多候选必填」那条分支 ——
/// 改 fixture 而不是回退新语义。
#[tokio::test]
async fn outsource_receive_requires_next_process_when_no_chain_40001() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-NOCHAIN", false, true).await;
    // 第二个候选（只把候选数抬到 2；无链 ⇒ 它在链外也无所谓）
    let extra = seed_process(&pool, "OR-NOCHAIN-PX", "候选工序2").await;
    link_work_type_to_process(&pool, st.work_type_id, extra).await;

    let (s, env) = receive(&app, &token, body(&st, 0, None)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "应 422: {env}");
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("next_process_id"),
        "文案应指明要在 next_process_id 里显式指定: {env}"
    );
    let (status, location, holder, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "40001 必须零写入");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(holder, Some(st.company_id));
    assert_eq!(version, 0);
    assert_eq!(count_part_events(&pool, st.part_id).await, 0);
}

/// 2026-10-16 新语义：无链 + 缺省 `next_process_id` + 工种**只映射一道** ⇒ 服务端
/// 自动推导，收回成功（工序 = 那一道）。
#[tokio::test]
async fn outsource_receive_auto_derives_single_work_type_process() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // setup_receive(..., with_chain=false, map_next=true) ⇒ 工种只映射「下一道」一道
    let st = setup_receive(&pool, "OR-AUTO1", false, true).await;

    let (s, env) = receive(&app, &token, body(&st, 0, None)).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "无链 + 工种单候选时不必让前端挑工序: {env}"
    );
    assert_eq!(
        env["data"]["next_process_id"],
        st.next_process_id.to_string(),
        "自动推导的工序必须是工种那唯一候选: {env}"
    );
    let (status, location, holder, process, _step, version, quantity) =
        read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(st.worker_id));
    assert_eq!(process, Some(st.next_process_id));
    assert_eq!(version, 2, "两段写各 +1");
    assert_eq!(quantity, 3);
}

/// 2026-10-16 新语义：工种**一条工序都没映射** + 缺省 `next_process_id` ⇒ `20104`
/// （HTTP 400，零写入），文案点名工号。
#[tokio::test]
async fn outsource_receive_empty_work_type_mapping_returns_20104() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // 工种存在但**不 link** 任何工序
    let st = setup_receive_without_process_mapping(&pool, "OR-EMPTY").await;

    let (s, env) = receive(&app, &token, body(&st, 0, None)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "零候选应 400: {env}");
    assert_eq!(env["code"], 20104, "BIZ_INVALID_VALUE: {env}");
    let msg = env["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains(&st.worker_id.to_string()) && msg.contains("未映射任何工序"),
        "文案应点名工号并说明「工种未映射任何工序」: {env}"
    );
    let (status, location, holder, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "20104 必须零写入");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(holder, Some(st.company_id));
    assert_eq!(version, 0);
    assert_eq!(count_part_events(&pool, st.part_id).await, 0);
}

/// 显式 `next_process_id` ∉ 扫码工种的活跃映射 → `20104`（HTTP 400，零写入）。
#[tokio::test]
async fn outsource_receive_manual_next_not_in_work_type_returns_20104() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // 无链 + 工种只映射当前工序：显式传「下一道」必被 20104 拒
    let st = setup_receive(&pool, "OR-WT", false, false).await;

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20104, "BIZ_INVALID_VALUE: {env}");
    let (status, _loc, _h, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "20104 必须零写入");
    assert_eq!(version, 0);
    assert_eq!(count_part_events(&pool, st.part_id).await, 0);
}

/// 有链但**当前工序 / 指针均被清空** + 缺省 `next_process_id` + 工种**多道**候选
/// → `20706`（HTTP 409，零写入）。
///
/// 出池清工序不变式下外协批次两列均为 NULL；此时锚链仍在（part 绑着链），写侧
/// `NEXT_PROCESS_LATERAL_SQL` 推不出下一道 ⇒ `20706`（两段文案的「有链推不出」段）。
/// 与 §8 残余失败组合第 4 条对位：显式回传 `next_process_id` 可绕过本形态。
///
/// ⚠️ 2026-10-16 调整 fixture：工种映射由 1 道改为 **2 道** —— 单候选时写侧会
/// 退回工种候选自动推导（成功），多候选才保留链推导自己的 `20706` 文案。
#[tokio::test]
async fn outsource_receive_chain_without_position_returns_20706() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-NOPOS", true, true).await;
    // 第二个候选（只把候选数抬到 2）
    let extra = seed_process(&pool, "OR-NOPOS-PX", "候选工序2").await;
    link_work_type_to_process(&pool, st.work_type_id, extra).await;
    sqlx::query(
        "UPDATE t_part_batch SET current_process_id = NULL, current_process_step_id = NULL \
         WHERE id = $1",
    )
    .bind(st.batch_id)
    .execute(&pool)
    .await
    .expect("clear process columns");
    assert!(
        part_chain_id(&pool, st.part_id).await.is_some(),
        "前置：工单仍绑着链（否则会落到无链的 40001 分支）"
    );

    let (s, env) = receive(&app, &token, body(&st, 0, None)).await;
    assert_eq!(s, StatusCode::CONFLICT, "20706 归 409 段: {env}");
    assert_eq!(env["code"], 20706, "BIZ_PROCESS_CHAIN_REQUIRED: {env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("无法推导下一道工序"),
        "有链推不出 ⇒ 文案应为「请手填」段: {env}"
    );
    let (status, location, holder, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "20706 必须零写入");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(holder, Some(st.company_id));
    assert_eq!(version, 0);
    assert_eq!(count_part_events(&pool, st.part_id).await, 0);
}

// ===========================================================================
//  stage① 选架失败（20508）的零写入
// ===========================================================================

/// 下一道工序**无可用生产架**（唯一映射被软删）→ stage① 选架落空返 `20508`
/// （HTTP 404，零写入）。
///
/// 判据 1–8 全部**先于任何写**：失败时批次仍 `OUTSOURCE + OUTSOURCE_COMPANY`、
/// 开口 shipment 仍 `OUTSOURCING`、零 part 事件、version 不变。
#[tokio::test]
async fn outsource_receive_no_shelf_for_next_process_returns_20508() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-NOSH", true, true).await;
    let (sid, _qid) = insert_open_shipment(
        &pool,
        st.part_id,
        st.batch_id,
        st.company_id,
        st.current_process_id,
    )
    .await;
    // 唯一映射「下一道」的货架映射软删 ⇒ 选架候选集为空（写侧 20508）。
    sqlx::query("UPDATE t_shelf_process SET deleted_at = now() WHERE shelf_id = $1")
        .bind(st.shelf_id)
        .execute(&pool)
        .await
        .expect("soft delete shelf mapping");

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "20508 归 404 段: {env}");
    assert_eq!(env["code"], 20508, "BIZ_SHELF_PROCESS_NOT_FOUND: {env}");
    // ---- 零写入逐项 ----
    let (status, location, holder, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "20508 必须零写入（批次状态）");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(holder, Some(st.company_id), "在途公司不变");
    assert_eq!(version, 0, "version 不变");
    let (shipment_status, received_at): (String, Option<chrono::NaiveDateTime>) =
        sqlx::query_as("SELECT status, received_at FROM t_outsource_shipment WHERE id = $1")
            .bind(sid)
            .fetch_one(&pool)
            .await
            .expect("read shipment");
    assert_eq!(shipment_status, "OUTSOURCING", "shipment 必须仍开口");
    assert_eq!(received_at, None);
    assert_eq!(
        count_part_events(&pool, st.part_id).await,
        0,
        "20508 必须零 part 事件"
    );
}

// ===========================================================================
//  链外工序 / `StepResolve::Lenient`（2026-10-16 review 第 1 轮 BLOCKER-1）
// ===========================================================================

/// **BLOCKER-1 回归**：工单**有链** + 显式 `next_process_id` 指向**链外**工序 ⇒
/// 收回**成功**，`current_process_step_id` 落 `NULL`（不再 `20702`）。
///
/// 改前 stage① 内核走 `optional_step_id`，「有链但链内定位不到」即抛 `20702` ——
/// 而本端点 2026-10-16 的新语义（工种候选**不与链求交**）恰恰会主动生产这个输入。
/// 断言三件事：HTTP 200、工序落显式值、step 为 `None`。
#[tokio::test]
async fn outsource_receive_explicit_process_outside_chain_succeeds_with_null_step() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-OUTCHAIN", true, true).await;
    // 链外工序：工种与货架都映射它，但**不 append 到工单的链上**
    let outside = seed_process(&pool, "OR-OUTCHAIN-PX", "链外工序").await;
    link_work_type_to_process(&pool, st.work_type_id, outside).await;
    link_shelf_to_process(&pool, st.shelf_id, outside).await;
    // 前置：工单确实有链，且该工序确实不在链内
    assert!(
        part_chain_id(&pool, st.part_id).await.is_some(),
        "前置：工单有链"
    );

    let (s, env) = receive(&app, &token, body(&st, 0, Some(outside))).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "有链 + 链外工序必须成功（step 落 NULL，不是 20702）: {env}"
    );
    assert_eq!(env["data"]["next_process_id"], outside.to_string());

    let (status, location, holder, process, step, version, quantity) =
        read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(st.worker_id));
    assert_eq!(process, Some(outside), "工序必须落显式指定的链外工序");
    assert_eq!(
        step, None,
        "链内定位不到 ⇒ step 落 NULL（Lenient），不是保持旧值、也不是报错"
    );
    assert_eq!(version, 2);
    assert_eq!(quantity, 3);
}

/// **BLOCKER-1 回归（决策 3 形态）**：工单**有链**但**链推不出**（当前工序 / step
/// 指针被清空）+ 工种**单候选**且该候选**不在链内** ⇒ 缺省 `next_process_id`
/// 自动推导成功、step 落 `NULL`。
///
/// 这一形态不是边角：`resolve_receive_next_process` 内部是 `sort_order = 当前 + 1`
/// 口径，**稀疏链上必然推不出** ⇒ 正好落进「退回工种候选」那条分支；而决策 2 又要求
/// 候选**不与链求交** ⇒ 「part 有链 + 单候选链外」这个组合是新语义主动生产出来的。
/// 改前它会被 stage① 的 `optional_step_id` 原样打回 `20702`。
#[tokio::test]
async fn outsource_receive_chain_derivation_fails_single_candidate_outside_chain_succeeds() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-CHAINFAIL", true, true).await;
    // 链外唯一候选：先软删 setup 里「下一道」那条映射，再只映射它
    let solo = seed_process(&pool, "OR-CHAINFAIL-PX", "链外唯一候选").await;
    sqlx::query(
        "UPDATE t_work_type_process SET deleted_at = now() \
         WHERE work_type_id = $1 AND process_id <> $2",
    )
    .bind(st.work_type_id)
    .bind(solo)
    .execute(&pool)
    .await
    .expect("soft drop other work type mappings");
    link_work_type_to_process(&pool, st.work_type_id, solo).await;
    link_shelf_to_process(&pool, st.shelf_id, solo).await;
    // 出池清工序不变式：当前工序与 step 指针都清空 ⇒ 链推不出下一道
    sqlx::query(
        "UPDATE t_part_batch SET current_process_id = NULL, current_process_step_id = NULL \
         WHERE id = $1",
    )
    .bind(st.batch_id)
    .execute(&pool)
    .await
    .expect("clear process columns");
    assert!(
        part_chain_id(&pool, st.part_id).await.is_some(),
        "前置：工单仍绑着链（否则落无链分支，测的不是本形态）"
    );

    let (s, env) = receive(&app, &token, body(&st, 0, None)).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "有链 + 链推不出 + 工种单候选（链外）⇒ 自动推导成功: {env}"
    );
    assert_eq!(env["data"]["next_process_id"], solo.to_string());

    let (status, location, holder, process, step, version, _q) =
        read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(st.worker_id));
    assert_eq!(process, Some(solo), "工序必须落工种那唯一候选");
    assert_eq!(step, None, "链外候选 ⇒ step 落 NULL");
    assert_eq!(version, 2);
}

// ===========================================================================
//  批次形态 / OCC / 工牌
// ===========================================================================

/// 批次 status 不是 `OUTSOURCE` → `20103`（HTTP 400，零写入）。
#[tokio::test]
async fn outsource_receive_wrong_status_20103() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-ST", true, true).await;
    force_batch_state(
        &pool,
        st.batch_id,
        "IN_PROCESS",
        Some("WORKER"),
        Some(st.worker_id),
    )
    .await;

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20103, "BIZ_INVALID_TRANSITION: {env}");
    let (status, _loc, _h, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "IN_PROCESS", "20103 必须零写入");
    assert_eq!(version, 0);
}

/// `OUTSOURCE + OUTSOURCE_COMPANY` 但 **holder 为 NULL**（数据异常）→ `20122`
/// （HTTP 409，零写入）。
///
/// 在途公司 = 批次行上的持有人；NULL 时写侧取不到 `company_id`，与「位置对不上」
/// 同类拒 `20122`（与 S1 `locate` 的 `locate_outsource_holder_null_returns_unknown`
/// 同一条订正）。
#[tokio::test]
async fn outsource_receive_holder_null_returns_20122() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-NOH", true, true).await;
    force_batch_state(
        &pool,
        st.batch_id,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        None,
    )
    .await;

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::CONFLICT, "20122 归 409 段: {env}");
    assert_eq!(env["code"], 20122, "BIZ_BATCH_LOCATION_MISMATCH: {env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("没有持有人"),
        "文案应指向「数据异常：没有持有人（公司）」: {env}"
    );
    let (status, location, holder, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "20122 必须零写入");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(holder, None, "holder 保持 NULL");
    assert_eq!(version, 0);
    assert_eq!(count_part_events(&pool, st.part_id).await, 0);
}

/// 批次 location 不是 `OUTSOURCE_COMPANY` → `20122`（HTTP 409，零写入）。
#[tokio::test]
async fn outsource_receive_wrong_location_20122() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-LOC", true, true).await;
    force_batch_state(
        &pool,
        st.batch_id,
        "OUTSOURCE",
        Some("PRODUCTION_SHELF"),
        Some(st.company_id),
    )
    .await;

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::CONFLICT, "20122 归 409 段: {env}");
    assert_eq!(env["code"], 20122, "BIZ_BATCH_LOCATION_MISMATCH: {env}");
    let (status, location, _h, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "20122 必须零写入");
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(version, 0);
}

/// 客户端 `version` 与批次不一致 → `40901`（HTTP 409，零写入）。
#[tokio::test]
async fn outsource_receive_version_conflict_40901() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-OCC", true, true).await;
    let (sid, _qid) = insert_open_shipment(
        &pool,
        st.part_id,
        st.batch_id,
        st.company_id,
        st.current_process_id,
    )
    .await;

    let (s, env) = receive(&app, &token, body(&st, 7, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");
    let (status, _loc, _h, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "40901 必须零写入");
    assert_eq!(version, 0);
    // shipment 也一行不动
    let shipment_status: String =
        sqlx::query_scalar("SELECT status FROM t_outsource_shipment WHERE id = $1")
            .bind(sid)
            .fetch_one(&pool)
            .await
            .expect("read shipment");
    assert_eq!(shipment_status, "OUTSOURCING");
}

/// 批次不存在（或已软删）→ `20109`（HTTP 404）。
#[tokio::test]
async fn outsource_receive_batch_not_found_20109() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-NOBATCH", true, true).await;

    let (s, env) = receive(&app, &token, {
        json!({
            "batch_id": MISSING_BATCH_ID.to_string(),
            "badge_code": st.badge_code,
            "version": 0,
            "next_process_id": st.next_process_id.to_string(),
        })
    })
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20109, "{env}");
}

/// 工牌未注册 → `20201`（HTTP 404）。
#[tokio::test]
async fn outsource_receive_badge_not_found_20201() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-BADGE", true, true).await;

    let (s, env) = receive(&app, &token, {
        let mut b = body(&st, 0, Some(st.next_process_id));
        b["badge_code"] = json!("NO-SUCH-BADGE");
        b
    })
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20201, "{env}");
}

/// 工人已停用 → `20202`（HTTP 400，零写入）。
#[tokio::test]
async fn outsource_receive_badge_inactive_20202() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-INACT", true, true).await;
    sqlx::query("UPDATE t_worker SET is_active = false WHERE id = $1")
        .bind(st.worker_id)
        .execute(&pool)
        .await
        .expect("deactivate worker");

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20202, "{env}");
    let (status, _loc, _h, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "20202 必须零写入");
    assert_eq!(version, 0);
}

/// 工人未分配工种 → `20206`（HTTP 400，零写入）。
#[tokio::test]
async fn outsource_receive_worker_without_work_type_20206() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "OR-NOWT-C").await;
    let proc = seed_process(&pool, "OR-NOWT-P", "工序").await;
    // 扫码工：active 但 work_type_id = NULL
    let worker = insert_worker(&pool, "OR-NOWT-W", "无工种", None).await;
    let company = insert_outsource_company(&pool, "OR-NOWT-公司").await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "OR-NOWT", worker, proc, 1, true).await;
    force_batch_state(
        &pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;

    let (s, env) = receive(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "OR-NOWT-W",
            "version": 0,
            "next_process_id": proc.to_string(),
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20206, "{env}");
    let (status, _loc, _h, _p, _step, version, _q) = read_batch(&pool, batch).await;
    assert_eq!(status, "OUTSOURCE", "20206 必须零写入");
    assert_eq!(version, 0);
}

// ===========================================================================
//  容量 / 角色
// ===========================================================================

/// **关键回归**：工人已持有 `max_held_batches` 批 → `20204`（HTTP 409）
/// 且**零写入**：批次保持 `OUTSOURCE + OUTSOURCE_COMPANY`、shipment 仍
/// `OUTSOURCING`、零 part 事件、version 不变。
///
/// 容量预检排在 stage① **之前**正是为了这个 —— 否则会留下「已收回落架、
/// 但没交出去」的半截状态。
#[tokio::test]
async fn outsource_receive_when_worker_full_20204_and_no_writes() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // max_held = 1
    let customer = insert_customer_l2(&pool, "OR-FULL-C").await;
    let proc_cur = seed_process(&pool, "OR-FULL-PC", "当前工序").await;
    let proc_next = seed_process(&pool, "OR-FULL-PN", "下一道").await;
    let wt = insert_work_type(&pool, "OR-FULL-WT", "工种", Some(1)).await;
    link_work_type_to_process(&pool, wt, proc_next).await;
    let worker = insert_worker(&pool, "OR-FULL-W", "扫码工", Some(wt)).await;
    let shelf = insert_shelf(&pool, "OR-FULL-SH", "生产架", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf, proc_next).await;
    let company = insert_outsource_company(&pool, "OR-FULL-公司").await;
    // 工人已经拿满 1/1
    insert_worker_held_part(&pool, customer, "OR-FULL-HELD", worker, proc_cur, 1, true).await;
    // 目标：外协在途批次（带开口 shipment）
    let (part, batch, _step) =
        insert_worker_held_part(&pool, customer, "OR-FULL-TARGET", worker, proc_cur, 3, true).await;
    force_batch_state(
        &pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;
    let (sid, _qid) = insert_open_shipment(&pool, part, batch, company, proc_cur).await;

    let (s, env) = receive(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "OR-FULL-W",
            "version": 0,
            "next_process_id": proc_next.to_string(),
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "容量触顶应 409: {env}");
    assert_eq!(env["code"], 20204, "BIZ_WORKER_HOLD_LIMIT_EXCEEDED: {env}");

    // ---- 零写入逐项 ----
    let (status, location, holder, _process, _step, version, _q) = read_batch(&pool, batch).await;
    assert_eq!(status, "OUTSOURCE", "20204 必须零写入（批次状态）");
    assert_eq!(location.as_deref(), Some("OUTSOURCE_COMPANY"));
    assert_eq!(holder, Some(company), "在途公司不变");
    assert_eq!(version, 0, "version 不变");
    let (shipment_status, received_at): (String, Option<chrono::NaiveDateTime>) =
        sqlx::query_as("SELECT status, received_at FROM t_outsource_shipment WHERE id = $1")
            .bind(sid)
            .fetch_one(&pool)
            .await
            .expect("read shipment");
    assert_eq!(shipment_status, "OUTSOURCING", "shipment 必须仍开口");
    assert_eq!(received_at, None);
    assert_eq!(
        count_part_events(&pool, part).await,
        0,
        "20204 必须零 part 事件"
    );
}

/// 角色门禁：CLERK → `40300`（Manager + ShelfAccount 才可），零写入。
#[tokio::test]
async fn outsource_receive_clerk_forbidden_40300() {
    let (pool, app, _manager, _fx) = bootstrap_as_manager().await;
    let clerk = login_token(
        &app,
        PartFixture::CLERK_USERNAME,
        ProductionFixture::PASSWORD,
    )
    .await;
    let st = setup_receive(&pool, "OR-CLERK", true, true).await;

    let (s, env) = receive(&app, &clerk, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "CLERK 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
    let (status, _loc, _h, _p, _step, version, _q) = read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "OUTSOURCE", "40300 必须零写入");
    assert_eq!(version, 0);
}

// ===========================================================================
//  shipment 关闭 / 事件日志
// ===========================================================================

/// 有开口 shipment：收回后 shipment 变 `RECEIVED` + `received_at` 非空 +
/// quote 事件 `RECEIVED` 落库。
#[tokio::test]
async fn outsource_receive_closes_shipment_and_quote_event() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-SHIP", true, true).await;
    let (sid, qid) = insert_open_shipment(
        &pool,
        st.part_id,
        st.batch_id,
        st.company_id,
        st.current_process_id,
    )
    .await;

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::OK, "{env}");

    let (status, received_at, version): (String, Option<chrono::NaiveDateTime>, i32) =
        sqlx::query_as(
            "SELECT status, received_at, version FROM t_outsource_shipment WHERE id = $1",
        )
        .bind(sid)
        .fetch_one(&pool)
        .await
        .expect("read shipment");
    assert_eq!(status, "RECEIVED");
    assert!(received_at.is_some(), "received_at 必须写上");
    assert_eq!(version, 1, "shipment 的 OCC 也应 +1");

    // quote 事件：`qid != 0` 时写 RECEIVED
    let (event_type, from_status, to_status, note): (String, String, String, Option<String>) =
        sqlx::query_as(
            "SELECT event_type, from_status, to_status, note FROM t_outsource_quote_event \
             WHERE quote_id = $1 AND event_type = 'RECEIVED'",
        )
        .bind(qid)
        .fetch_one(&pool)
        .await
        .expect("应有 1 条 quote RECEIVED 事件");
    assert_eq!(event_type, "RECEIVED");
    assert_eq!(from_status, "APPROVED");
    assert_eq!(to_status, "APPROVED");
    assert_eq!(note, None, "本端点 body 无 note ⇒ 事件备注为空");
}

/// 无开口 shipment 的 legacy 形态（外协厂直接入库的批次）：关闭逻辑静默跳过，
/// 端点照常成功。
#[tokio::test]
async fn outsource_receive_without_open_shipment_still_succeeds() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // setup 不插 shipment
    let st = setup_receive(&pool, "OR-NOSHIP", true, true).await;

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::OK, "无开口 shipment 不算错: {env}");
    let (status, location, holder, process, _step, version, _q) =
        read_batch(&pool, st.batch_id).await;
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(st.worker_id));
    assert_eq!(process, Some(st.next_process_id));
    assert_eq!(version, 2);
}

/// `t_part_event` 两条事件逐字段：`RECEIVED_FROM_OUTSOURCE`（收回）+ `PICKED_UP`
/// （交到工人手上），顺序按 id 升序。
#[tokio::test]
async fn outsource_receive_writes_both_part_events() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let st = setup_receive(&pool, "OR-EV", true, true).await;

    let (s, env) = receive(&app, &token, body(&st, 0, Some(st.next_process_id))).await;
    assert_eq!(s, StatusCode::OK, "{env}");

    let rows: Vec<PartEventRow> = sqlx::query_as(
        "SELECT event_type, from_status, to_status, quantity, worker_id, process_id, note \
         FROM t_part_event WHERE part_id = $1 ORDER BY id ASC",
    )
    .bind(st.part_id)
    .fetch_all(&pool)
    .await
    .expect("read t_part_event");
    assert_eq!(rows.len(), 2, "应恰有两条事件: {rows:?}");

    // ① 收回：OUTSOURCE → IN_PROCESS，无工人 / 无工序快照（系统动作）
    let (et, from, to, qty, worker_id, process_id, note) = &rows[0];
    assert_eq!(et, "RECEIVED_FROM_OUTSOURCE");
    assert_eq!(from.as_deref(), Some("OUTSOURCE"));
    assert_eq!(to.as_deref(), Some("IN_PROCESS"));
    assert_eq!(*qty, Some(3));
    assert_eq!(*worker_id, None);
    assert_eq!(*process_id, None);
    assert_eq!(*note, None);

    // ② 交人：IN_PROCESS → IN_PROCESS，工人 = 扫码工，工序快照 = 目标工序
    let (et, from, to, qty, worker_id, process_id, note) = &rows[1];
    assert_eq!(et, "PICKED_UP");
    assert_eq!(from.as_deref(), Some("IN_PROCESS"));
    assert_eq!(to.as_deref(), Some("IN_PROCESS"));
    assert_eq!(*qty, Some(3));
    assert_eq!(*worker_id, Some(st.worker_id));
    assert_eq!(*process_id, Some(st.next_process_id));
    assert_eq!(*note, None);
}
