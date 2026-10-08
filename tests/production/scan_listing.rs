//! `prod::scan` 报工台两条只读聚合端点的出参集成测试（2026-10-03 新增，2026-10-10 迁入）
//!
//! 覆盖 `GET /api/v2/prod/scan/pickable?work_type_id=` 与
//! `GET /api/v2/prod/scan/held?worker_id=`。这两个端点此前在 `tests/` 下**零覆盖**
//! （只在别的文件的注释里被提到）；它们自 2026-10-04 起是报工台三页的**唯一数据源**，
//! 故在此补齐字段级契约：`batch_id` / `batch_version` 必须等于该行
//! `t_part_batch.id` / `t_part_batch.version`；`chain_*` 四字段只有 `held` 侧有值。
//!
//! 2026-10-10 自 `tests/part/pickable_by_work_type.rs` 迁来（URL 全部硬切，无 alias），
//! 同时吸收 `tests/part/lifecycle.rs` 里「工种 / 工人维度三条 list 端点的 part 侧真实
//! 字段投影」一节里属于这两条端点的两例。留在 part 域的 `GET /parts/by-work-type/{id}`
//! 仍由 `tests/part/lifecycle.rs` 覆盖。
//!
//! ## 过滤条件（全套都满足，行才进列表）
//! **pickable**：批次 `status='IN_PROCESS'` + `location='PRODUCTION_SHELF'` +
//! `deleted_at IS NULL`，挂在 `zone='PRODUCTION'` 且 `is_active=true` 且**未软删**的
//! 货架上，且批次 `current_process_id` 命中 `t_work_type_process` 里该工种的**活跃**
//! 映射；另需 part 本身 `deleted_at IS NULL`。货架范围再按当前账号 scope 收窄。
//!
//! **held**：批次 `status='IN_PROCESS'` + `location='WORKER'` + `deleted_at IS NULL`
//! 且 `current_holder_id = worker_id`；另需 part 本身 `deleted_at IS NULL`。不做货架
//! scope 收窄（行已在工人手上，不是架上的候选池）。
//!
//! fixture 复用 `ProductionFixture`（它内部先 `load_part_fixture`，故 `FX-PROC-A`
//! 工序 / `FX-WT-A` 工种 / `FX-WTA ↔ FX-PROC-A` 映射都是预置的）。生产货架取
//! `PartFixture::PRODUCTION_SHELF_ID` 常量（`ProductionFixture` 只保留 part 域基线的
//! 几个 id，没转出 production shelf）。本文件只需直插 part / batch / 附加货架。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::fixture::{PartFixture, ProductionFixture};
use hsh_erp_test_support::{
    json_request, load_production_fixture, login_token, pool_snowflake, send, send_raw, test_app,
    test_pool, test_state,
};

/// 可领取列表路径前缀（测试 app 不带 `/api/v2`）。2026-10-10 起 `work_type_id` 是
/// **query 参数**（旧路径是 path 段）。
const PICKABLE_URI_PREFIX: &str = "/prod/scan/pickable";
/// 工人持有列表路径前缀（测试 app 不带 `/api/v2`）。
const HELD_URI_PREFIX: &str = "/prod/scan/held";

/// fixture 预置的 active PRODUCTION 货架（`FX-SH-PROD`）。
const PRODUCTION_SHELF_ID: i64 = PartFixture::PRODUCTION_SHELF_ID;

// ===========================================================================
//  Bootstrap helpers
// ===========================================================================

/// fresh database + production fixture + 以 MANAGER 身份登录。
async fn bootstrap() -> (PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  本 sub-file 独享 raw SQL 构造
// ===========================================================================

/// 插一个 `t_part` 行（`applicant_name` NOT NULL → 空串占位）。
///
/// `serial_no` 走 `Option` 形参：`t_part.serial_no` 是 nullable（手工工单无序列号），
/// 传 `None` 的用例顺带锁住取行 SQL 必须按 `Option<String>` 解码。
async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    name: &str,
    drawing_no: &str,
    serial_no: Option<&str>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, status, is_urgent, customer_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, '', 2, $5, $5, 'IN_PROCESS', false, $6, 0, $7, $7)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(drawing_no)
    .bind(now.date())
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 插一个「可领取」批次：PRODUCTION_SHELF + holder 指向 PRODUCTION 货架 +
/// `current_process_id` 指向该工种映射的工序；`version` 由入参指定（供 OCC 断言）。
async fn insert_pickable_batch(
    pool: &PgPool,
    part_id: i64,
    shelf_id: i64,
    current_process_id: i64,
    version: i32,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, current_process_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 2, 'IN_PROCESS', 'PRODUCTION_SHELF', $3, $4, $5, $6, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(shelf_id)
    .bind(current_process_id)
    .bind(version)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 插一张 active 的 PRODUCTION 货架（`zone='PRODUCTION'` 是本端点的硬过滤条件）。
async fn insert_production_shelf(pool: &PgPool, code: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) VALUES ($1, $2, $2, 'PRODUCTION', true, 0, 0, $3, $3)",
    )
    .bind(id)
    .bind(code)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

/// 回读批次在库里的 `id` / `version`（断言出参 = 库里真值，避免自证）。
async fn batch_id_version_in_db(pool: &PgPool, batch_id: i64) -> (i64, i32) {
    let (id, v): (i64, i32) = sqlx::query_as("SELECT id, version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("select t_part_batch id/version id={batch_id}: {e}"));
    (id, v)
}

// ===========================================================================
//  断言 helper
// ===========================================================================

/// 打一次可领取列表端点，返回信封（已 assert 200 + code 0）。
async fn get_pickable(app: &axum::Router, token: &str, work_type_id: i64) -> Value {
    let uri = format!("{PICKABLE_URI_PREFIX}?work_type_id={work_type_id}");
    let (status, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(env["code"], 0, "GET {uri}: {env}");
    env
}

/// 从信封里按 part id 取单条 item（找不到则 panic 并打印整份信封）。
fn item_by_part_id(env: &Value, part_id: i64) -> &Value {
    let want = part_id.to_string();
    env["data"]["items"]
        .as_array()
        .expect("data.items")
        .iter()
        .find(|it| it["id"].as_str() == Some(want.as_str()))
        .unwrap_or_else(|| panic!("part {part_id} 不在结果里: {env}"))
}

/// 列出信封里全部 item 的 part id（雪花 i64 → string）。
fn part_ids(env: &Value) -> Vec<String> {
    env["data"]["items"]
        .as_array()
        .expect("data.items")
        .iter()
        .map(|it| it["id"].as_str().expect("item.id 是 string").to_string())
        .collect()
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 2026-10-03 新增：`batch_id` / `batch_version` 与库里的 `t_part_batch` 逐字一致。
///
/// `batch_version` 刻意用非 0 值（4）：默认 0 的话，「取到真值」与「没取到而兜底 0」
/// 无法区分。
#[tokio::test]
async fn pickable_returns_batch_id_and_version_matching_db() {
    let (pool, app, token, fx) = bootstrap().await;
    // serial_no = NULL：手工工单形态。2026-10-03 之前本端点按 `String` 解码
    // `p.serial_no`，这种行会让整页 500，故本用例同时锁住「nullable 不炸」。
    let part_id = insert_part(&pool, fx.part_customer_l1_id, "可领件A", "D-PICK-A", None).await;
    let batch_id =
        insert_pickable_batch(&pool, part_id, PRODUCTION_SHELF_ID, fx.process_a_id, 4).await;
    assert_eq!(
        batch_id_version_in_db(&pool, batch_id).await,
        (batch_id, 4),
        "前提：库里的批次真值就是 (batch_id, version=4)"
    );

    let env = get_pickable(&app, &token, fx.work_type_a_id).await;
    let item = item_by_part_id(&env, part_id);
    assert_eq!(
        item["batch_id"].as_str(),
        Some(batch_id.to_string().as_str()),
        "batch_id 必须是雪花 ID 的 JSON string 形态且等于 t_part_batch.id: {env}"
    );
    assert_eq!(
        item["batch_version"], 4,
        "batch_version 必须等于 t_part_batch.version: {env}"
    );
    // 2026-10-10：行 VO 已收敛成 `ScanListItem`，**没有** part 级 `version` 字段
    // （它在本 VO 上恒为 0 占位，取行 SQL 也从不投影 `p.version`）。批次 OCC 只认
    // `batch_version`；留着 `version` 只会让两个「版本」继续被混用。
    assert!(
        item.get("version").is_none(),
        "行 VO 不该再带 part 级 version 占位字段: {item}"
    );
    // 行内其余字段仍由手工 TPart 占位派生：quantity 取自批次、drawing_no 来自 part
    assert_eq!(item["quantity"], 2, "quantity 取自批次而非 part: {env}");
    assert_eq!(item["drawing_no"], "D-PICK-A", "{env}");
}

// ===========================================================================
//  2026-10-03 review 第 1 轮 Major-2：`serial_no IS NULL` 不得整页 500
// ===========================================================================
//
// `t_part.serial_no` 是 nullable（手工工单无序列号；baseline 里是
// `serial_no character varying(15)`，无 NOT NULL）。本文件三个同族端点的取行 SQL
// 此前都把 `p.serial_no` 按 `String` 解码，遇任一 `serial_no IS NULL` 的 part 就
// `ColumnDecode` → 整页 500（`unexpected null; try decoding as an Option`）。
// 手工工单是常态 ⇒ 这是必经路径而非边角。
//
// 本轮（独立 commit）一次清完 `part/service/phase1/work_type.rs` 的全部 3 处：
//   · 报工台 pickable 列表（`GET /prod/scan/pickable?work_type_id=`）
//   · 报工台 held 列表（`GET /prod/scan/held?worker_id=`）        ← 本节用例 2
// 同族的 `list_by_work_type`（`GET /parts/by-work-type/{id}`）留在 part 域，用例在
// `tests/part/lifecycle.rs`。

/// 插一个 active 且已绑 work_type 的工人（`by-work-type` 走 `t_worker` JOIN，
/// 报工台 held 列表以 worker_id 为过滤锚点）。
async fn insert_active_worker(pool: &PgPool, work_type_id: i64, code: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) VALUES ($1, $2, $3, true, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(code)
    .bind(format!("{code}-NAME"))
    .bind(work_type_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

/// 插一个「工人持有中」的批次（`IN_PROCESS` + `location='WORKER'` + holder=worker），
/// 这是 `by-work-type` 与报工台 held 列表的硬过滤条件。
async fn insert_worker_held_batch(pool: &PgPool, part_id: i64, worker_id: i64, qty: i32) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, $3, 'IN_PROCESS', 'WORKER', $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(part_id)
    .bind(qty)
    .bind(worker_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch (WORKER-held)");
    id
}

/// 2026-10-03 review 第 1 轮 Major-2 用例 2：报工台 held 列表同款。
#[tokio::test]
async fn by_worker_tolerates_null_serial_no() {
    let (pool, app, token, fx) = bootstrap().await;
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-SERIAL-NULL").await;
    let manual_part = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "持有中无序列号件",
        "D-WK-SERIAL-NULL",
        None,
    )
    .await;
    let _manual_batch = insert_worker_held_batch(&pool, manual_part, worker_id, 7).await;

    // 另一个工人工种下也放一个有序列号的行，验证 worker_id 过滤仍生效
    let other_worker = insert_active_worker(&pool, fx.work_type_a_id, "WK-OTHER").await;
    let other_part = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "他人持有件",
        "D-WK-OTHER",
        Some("WK-OTHER-001"),
    )
    .await;
    let _other_batch = insert_worker_held_batch(&pool, other_part, other_worker, 2).await;

    let uri = format!("{HELD_URI_PREFIX}?worker_id={worker_id}");
    let (status, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "serial_no=NULL 不应 500: {env}");
    assert_eq!(env["code"], 0, "{uri}: {env}");

    assert_eq!(
        part_ids(&env),
        vec![manual_part.to_string()],
        "只返该工人持有的行（修复前整页 500）: {env}"
    );
    let item = item_by_part_id(&env, manual_part);
    assert!(
        item["serial_no"].is_null(),
        "手工工单的 serial_no 应序列化为 null: {env}"
    );
    assert_eq!(item["quantity"], 7, "quantity 取自批次: {env}");
}

// ===========================================================================
//  2026-10-04：报工台 held 列表的工序链派生
// ===========================================================================
//
// 报工台「放回」页要判定三态，全部依赖本端点行内字段（前端无第二数据源）：
//
// | `chain_state` | 含义                                   | 前端动作                        |
// |---------------|----------------------------------------|---------------------------------|
// | `NONE`        | 无链 / 链已软删 / 当前工序不在链内 / 链内 `process_id` 重复（位置有歧义） | 弹工序选择框，让用户手填 |
// | `NEXT`        | 当前工序在链内且有下一道                | 免填，确认后直接放回            |
// | `TAIL`        | 当前工序是链内最后一道                  | 提示「加工完成后请送检」        |
//
// 本节同时锁住批次锚点（`batch_id` / `batch_version`）—— 放回页要发写请求，
// 拿不到批次 id 就发不出去。
//
// ## 链数据怎么造
// `ProcessChainFixture` 只预置「2 工序 + 2 part + 2 用户」，**不含任何链 / step 行**
// （见 `test-support/src/fixture/process_chain.rs` 的「当前域」），故链数据在本节
// 按 DB 约定现场直插：`t_part_process_chain` + `t_process_chain_step`（无物理外键，
// 软删列留 NULL = 活跃）。`sort_order` 一律用稀疏 `10/20/30` —— 读侧「下一道」按
// `sort_order > 当前` 取（与写侧 `next_step_in_chain` 同形），稀疏链是它的**判别性**
// 输入：写成 `+ 1` 时这两组用例全塌成 `TAIL`。
//
// ## 关键回归点
// - `by_worker_chain_state_repositions_by_current_process_id_not_step_pointer`：
//   「不能拿 step 指针的 `sort_order` 当位置」—— worker-scan 的 RETURNED 只写
//   `current_process_id`、不推进 `current_process_step_id`，多工序链批次第 2 次放回
//   时指针仍停在**首次定位**那一步，按位置推进会把当前工序自己当成下一道返回。
// - `by_worker_chain_state_none_when_duplicate_process_in_chain`：链内同一
//   `process_id` 重复时「当前 step」定位扇行 ⇒ 显式降级 `NONE`，不许让 `LIMIT 1`
//   静默取到「`NEXT → 当前工序自己`」那一行。

/// 进程级 snowflake 取号（复用本文件既有 inline 写法，含毒化兜底）。
fn next_id() -> i64 {
    pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id()
}

/// 插一个 INHOUSE 工序（链里有 3 道，而 fixture 只预置 2 道；漂移用例还要第 4 道）。
async fn insert_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = next_id();
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

/// 建一条空工艺链（不绑 part），返回 chain_id。
async fn create_chain(pool: &PgPool, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, $2, 0, $3, 0, $3, 0)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_process_chain");
    id
}

/// 往链上追加一个 step，返回 step_id。
async fn add_chain_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 30, 0, $5, 0, $5, 0)",
    )
    .bind(id)
    .bind(chain_id)
    .bind(sort_order)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process_chain_step");
    id
}

/// 把 part 绑到指定工艺链（`uq_t_part_process_chain` 要求一条活跃链只绑一个 part，
/// 故同一测试里只能建一条链绑一个 part）。
async fn bind_part_to_chain(pool: &PgPool, part_id: i64, chain_id: i64) {
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("bind part to chain");
}

/// 写批次的链位置两列（`insert_worker_held_batch` 造的批次这两列是 NULL）。
///
/// 两列**故意分开**给：worker-scan 的 RETURNED 只写 `current_process_id` 而不推进
/// `current_process_step_id`，所以「指针漂移 + 工序正确」是生产上真实存在的组合。
async fn set_batch_position(
    pool: &PgPool,
    batch_id: i64,
    current_process_id: Option<i64>,
    current_process_step_id: Option<i64>,
) {
    sqlx::query(
        "UPDATE t_part_batch SET current_process_id = $2, current_process_step_id = $3 \
         WHERE id = $1",
    )
    .bind(batch_id)
    .bind(current_process_id)
    .bind(current_process_step_id)
    .execute(pool)
    .await
    .expect("update t_part_batch chain position");
}

/// 打一次 held 列表并按 part id 取出目标行（找不到即 panic 并打印整份信封）。
async fn by_worker_item(app: &axum::Router, token: &str, worker_id: i64, part_id: i64) -> Value {
    let uri = format!("{HELD_URI_PREFIX}?worker_id={worker_id}");
    let (status, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(env["code"], 0, "GET {uri}: {env}");
    item_by_part_id(&env, part_id).clone()
}

/// 断言「取行 SQL 投影的 `b.id` / `b.version`」已填进出参。
fn assert_batch_anchor(item: &Value, batch_id: i64) {
    assert_eq!(
        item["batch_id"].as_str(),
        Some(batch_id.to_string().as_str()),
        "batch_id 必须是雪花 ID 的 JSON string 形态且等于 t_part_batch.id: {item}"
    );
    assert_eq!(
        item["batch_version"], 0,
        "batch_version 必须等于 t_part_batch.version（insert_worker_held_batch 写死 0）: {item}"
    );
    assert!(
        item.get("version").is_none(),
        "行 VO 不该再带 part 级 version 占位字段，批次 OCC 只认 batch_version: {item}"
    );
}

/// 断言「`chain_state == "NONE"`」的配套不变量：下一道 id 为 `"0"`、两个名字为
/// `null`。`NONE` 是保守降级态，前端据此弹工序选择框，任何一个派生字段残留真值
/// 都会让「免填」与「手填」两条路径在前端产生分歧。
fn assert_none_state(item: &Value) {
    assert_eq!(item["chain_state"], "NONE", "{item}");
    assert_eq!(
        item["chain_next_process_id"].as_str(),
        Some("0"),
        "NONE 时下一道工序 id 必须是 \"0\" 兜底（不是 null、也不是残留真值）: {item}"
    );
    assert!(
        item["chain_next_process_name"].is_null(),
        "NONE 时下一道工序名恒 null: {item}"
    );
    assert!(
        item["chain_current_process_name"].is_null(),
        "NONE 时当前工序名恒 null: {item}"
    );
}

/// 场景 1（`NONE` · 无链）：`p.process_chain_id IS NULL` ⇒ 前端弹工序选择框。
///
/// 同时锁住批次锚点与 `process_chain_id` 投影。
#[tokio::test]
async fn by_worker_chain_state_none_when_part_has_no_chain() {
    let (pool, app, token, fx) = bootstrap().await;
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-CHAIN-NONE").await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "无链条件",
        "D-CHAIN-NONE",
        Some("CHAIN-NONE-001"),
    )
    .await;
    let batch_id = insert_worker_held_batch(&pool, part_id, worker_id, 4).await;
    // 批次停在工序 A 上，但零件没制定工艺链 ⇒ 锚链解析失败
    set_batch_position(&pool, batch_id, Some(fx.process_a_id), None).await;

    let item = by_worker_item(&app, &token, worker_id, part_id).await;
    assert_none_state(&item);
    assert!(
        item["process_chain_id"].is_null(),
        "process_chain_id 取真实投影值（无链 ⇒ null）: {item}"
    );
    assert_batch_anchor(&item, batch_id);
}

/// 场景 2（`NEXT`）：链 = [A(10), B(20), C(30)]，当前工序 = B ⇒ 免填、下一道 = C。
///
/// 顺带锁住两个名字字段与 `process_chain_id` 的真实投影。
#[tokio::test]
async fn by_worker_chain_state_next_points_to_step_after_current_process() {
    let (pool, app, token, fx) = bootstrap().await;
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-CHAIN-NEXT").await;
    let proc_c = insert_process(&pool, "FX-NC-CHAIN", "链上第三道 NC").await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "链中段件",
        "D-CHAIN-NEXT",
        Some("CHAIN-NEXT-001"),
    )
    .await;
    let chain_id = create_chain(&pool, "chain-abc").await;
    bind_part_to_chain(&pool, part_id, chain_id).await;
    let _step_a = add_chain_step(&pool, chain_id, fx.process_a_id, 10).await;
    let step_b = add_chain_step(&pool, chain_id, fx.process_b_id, 20).await;
    let _step_c = add_chain_step(&pool, chain_id, proc_c, 30).await;
    let batch_id = insert_worker_held_batch(&pool, part_id, worker_id, 6).await;
    // 正常形态：step 指针与 current_process_id 同指 B
    set_batch_position(&pool, batch_id, Some(fx.process_b_id), Some(step_b)).await;

    let item = by_worker_item(&app, &token, worker_id, part_id).await;
    assert_eq!(item["chain_state"], "NEXT", "链中段 ⇒ NEXT: {item}");
    assert_eq!(
        item["chain_next_process_id"].as_str(),
        Some(proc_c.to_string().as_str()),
        "下一道必须是 sort_order 30 那道（工序 C）: {item}"
    );
    assert_eq!(
        item["chain_next_process_name"], "链上第三道 NC",
        "下一道工序名取 t_process.name: {item}"
    );
    assert_eq!(
        item["chain_current_process_name"], "FX 工序 NB",
        "当前工序名 = 批次 current_process_id 对应的工序名（字面值取自 \
         test-support/fixtures/production.sql 的 FX-NB 行）: {item}"
    );
    assert_eq!(
        item["process_chain_id"].as_str(),
        Some(chain_id.to_string().as_str()),
        "process_chain_id 投影真实链 id: {item}"
    );
    assert_batch_anchor(&item, batch_id);
}

/// 场景 3（`TAIL` · 链尾）：链 = [A, B, C]，当前工序 = C ⇒ 提示「加工完成后请送检」。
#[tokio::test]
async fn by_worker_chain_state_tail_when_current_process_is_chain_end() {
    let (pool, app, token, fx) = bootstrap().await;
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-CHAIN-TAIL").await;
    let proc_c = insert_process(&pool, "FX-NC-TAIL", "链尾工序 NC").await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "链尾件",
        "D-CHAIN-TAIL",
        Some("CHAIN-TAIL-001"),
    )
    .await;
    let chain_id = create_chain(&pool, "chain-abc").await;
    bind_part_to_chain(&pool, part_id, chain_id).await;
    add_chain_step(&pool, chain_id, fx.process_a_id, 10).await;
    add_chain_step(&pool, chain_id, fx.process_b_id, 20).await;
    let step_c = add_chain_step(&pool, chain_id, proc_c, 30).await;
    let batch_id = insert_worker_held_batch(&pool, part_id, worker_id, 5).await;
    set_batch_position(&pool, batch_id, Some(proc_c), Some(step_c)).await;

    let item = by_worker_item(&app, &token, worker_id, part_id).await;
    assert_eq!(item["chain_state"], "TAIL", "链尾 ⇒ TAIL: {item}");
    assert_eq!(
        item["chain_next_process_id"].as_str(),
        Some("0"),
        "链尾没有下一道 ⇒ 0 兜底: {item}"
    );
    assert!(
        item["chain_next_process_name"].is_null(),
        "TAIL 时下一道工序名恒 null: {item}"
    );
    assert_eq!(
        item["chain_current_process_name"], "链尾工序 NC",
        "TAIL 提示要能点名当前工序: {item}"
    );
    assert_batch_anchor(&item, batch_id);
}

/// 场景 4（`NONE` · 位置漂移）：链 = [A, B, C]，当前工序 = **链外的 D**。
///
/// 链本身可解析、step 指针也合法（指向 A），但「当前工序在链内的位置」定位失败
/// ⇒ 必须落 `NONE` 让用户手填，而不是退化成「按 A 的位置给 B」。
#[tokio::test]
async fn by_worker_chain_state_none_when_current_process_not_in_chain() {
    let (pool, app, token, fx) = bootstrap().await;
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-CHAIN-DRIFT").await;
    let proc_c = insert_process(&pool, "FX-NC-DRIFT", "链上第三道 NC").await;
    let proc_d = insert_process(&pool, "FX-ND-DRIFT", "链外工序 ND").await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "位置漂移件",
        "D-CHAIN-DRIFT",
        Some("CH-DRIFT-1"),
    )
    .await;
    let chain_id = create_chain(&pool, "chain-abc").await;
    bind_part_to_chain(&pool, part_id, chain_id).await;
    let step_a = add_chain_step(&pool, chain_id, fx.process_a_id, 10).await;
    add_chain_step(&pool, chain_id, fx.process_b_id, 20).await;
    add_chain_step(&pool, chain_id, proc_c, 30).await;
    let batch_id = insert_worker_held_batch(&pool, part_id, worker_id, 3).await;
    // 指针合法（指向 A），但 current_process_id 是链外的 D
    set_batch_position(&pool, batch_id, Some(proc_d), Some(step_a)).await;

    let item = by_worker_item(&app, &token, worker_id, part_id).await;
    assert_none_state(&item);
    assert_batch_anchor(&item, batch_id);
}

/// 场景 5（**核心回归**）：step 指针漂移但 `current_process_id` 正确时，
/// 下一道必须按 `current_process_id` 在链内重新定位。
///
/// 前置：链 = [A(10), B(20), C(30)]；`current_process_step_id` 仍指向 **A** 的 step，
/// `current_process_id = B`。这正是 worker-scan RETURNED 之后的形态（该分支只写
/// `current_process_id = next_process_id`、不推进 step 指针）。
///
/// 期望：`NEXT` + 下一道 = **C**。若实现改成拿 step 指针的 `sort_order` 当位置，
/// 会得到 sort 20 那道 = **B 自己**（即当前工序），前端仍在说「可免填」⇒ 静默把
/// 工件投回原工序。断言里额外 `assert_ne` 显式锁死这一点。
#[tokio::test]
async fn by_worker_chain_state_repositions_by_current_process_id_not_step_pointer() {
    let (pool, app, token, fx) = bootstrap().await;
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-CHAIN-REPOINT").await;
    let proc_c = insert_process(&pool, "FX-NC-REPOINT", "真正下一道 NC").await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "指针漂移件",
        "D-CHAIN-REPOINT",
        Some("CH-REPOINT-1"),
    )
    .await;
    let chain_id = create_chain(&pool, "chain-abc").await;
    bind_part_to_chain(&pool, part_id, chain_id).await;
    let step_a = add_chain_step(&pool, chain_id, fx.process_a_id, 10).await;
    add_chain_step(&pool, chain_id, fx.process_b_id, 20).await;
    add_chain_step(&pool, chain_id, proc_c, 30).await;
    let batch_id = insert_worker_held_batch(&pool, part_id, worker_id, 8).await;
    // ⚠️ 指针停在 A（sort 10），工序却是 B ⇒ 按 sort_order 推进会返回 B 自己
    set_batch_position(&pool, batch_id, Some(fx.process_b_id), Some(step_a)).await;

    let item = by_worker_item(&app, &token, worker_id, part_id).await;
    assert_eq!(
        item["chain_state"], "NEXT",
        "current_process_id=B 在链内且有下一道 ⇒ NEXT: {item}"
    );
    assert_eq!(
        item["chain_next_process_id"].as_str(),
        Some(proc_c.to_string().as_str()),
        "必须按 current_process_id=B 在链内定位后取 sort 30（工序 C）: {item}"
    );
    assert_ne!(
        item["chain_next_process_id"].as_str(),
        Some(fx.process_b_id.to_string().as_str()),
        "绝不能把当前工序自己（B）当成下一道返回: {item}"
    );
    assert_eq!(
        item["chain_next_process_name"], "真正下一道 NC",
        "下一道工序名与 id 同行: {item}"
    );
    assert_batch_anchor(&item, batch_id);
}

/// 场景 6（**歧义**）：链内同一 `process_id` 出现两次 ⇒ 必须落 `NONE`。
///
/// 前置：链 = [A(10), **A**(20), B(30)]，`current_process_id = A`、step 指针指向
/// A 的第一步。这条链是后端**照收**的：`t_process_chain_step` 只有
/// `uq_chain_step_chain_order (chain_id, sort_order)` 一个唯一约束，没有
/// `(chain_id, process_id)` 唯一约束；写侧 `upsert_chain` 也只校验链内
/// `sort_order` 互不重复。
///
/// 危险在于：按 `process_id` 定位当前 step 会**扇出两行** —— 一行派生
/// `NEXT → A`（**当前工序自己**），另一行派生 `TAIL`。若让 `LIMIT 1` 静默取其
/// 一，就是拿「绝不能把当前工序自己当成下一道」这条承诺去赌 PG 的行序。
///
/// 期望：`NONE`（`hit_count > 1` 显式降级）+ 下一道 id 为 `"0"`，前端弹工序
/// 选择框让工人手填。**不是 `TAIL`**：`TAIL` 会让放回页提示「加工完成后请送检」而不再
/// 要下一道工序，等于把「位置有歧义」当成「确定在链尾」。本用例即该安全承诺的边界
/// 守卫。
#[tokio::test]
async fn by_worker_chain_state_none_when_duplicate_process_in_chain() {
    let (pool, app, token, fx) = bootstrap().await;
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-CHAIN-DUP").await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "工序重复件",
        "D-CHAIN-DUP",
        Some("CH-DUP-1"),
    )
    .await;
    let chain_id = create_chain(&pool, "chain-abc").await;
    bind_part_to_chain(&pool, part_id, chain_id).await;
    // ⚠️ 工序 A 连上两道（sort 10 / 20）—— 唯一索引只管 sort 槽位唯一，管不到
    // process_id，故这条链能落库
    let step_a1 = add_chain_step(&pool, chain_id, fx.process_a_id, 10).await;
    add_chain_step(&pool, chain_id, fx.process_a_id, 20).await;
    add_chain_step(&pool, chain_id, fx.process_b_id, 30).await;
    let batch_id = insert_worker_held_batch(&pool, part_id, worker_id, 5).await;
    set_batch_position(&pool, batch_id, Some(fx.process_a_id), Some(step_a1)).await;

    // 前提自证：库里确实有 2 条 process_id = A 的活跃 step
    let dup_hits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM t_process_chain_step \
         WHERE chain_id = $1 AND process_id = $2 AND deleted_at IS NULL",
    )
    .bind(chain_id)
    .bind(fx.process_a_id)
    .fetch_one(&pool)
    .await
    .expect("count duplicate steps");
    assert_eq!(dup_hits, 2, "前提：锚链内 process_id=A 的活跃 step 有 2 条");

    let item = by_worker_item(&app, &token, worker_id, part_id).await;
    assert_none_state(&item);
    assert_ne!(
        item["chain_next_process_id"].as_str(),
        Some(fx.process_a_id.to_string().as_str()),
        "绝不能把当前工序自己（A）当成下一道返回: {item}"
    );
    assert_batch_anchor(&item, batch_id);
}

// ===========================================================================
//  2026-10-04：SHELF_ACCOUNT 货架 scope 收口
// ===========================================================================
//
// 2026-10-04 之前本端点**完全没有**按 `user.shelf_ids` 收口：全文零
// `can_access_shelf` / 零 `current.shelf_ids` / 零 `shelf_wildcard`，唯一的货架
// 输入是客户端可控的 `?shelf_id=`，而它不与用户 scope 求交、不传时谓词恒真。
// ⇒ 绑了架 A 的 SHELF_ACCOUNT 能看到**全厂所有 PRODUCTION 架**上该工种可领的批次。
//
// 2026-10-10 起 `?shelf_id=` 这个入参本身也删掉了（8 条写路径都不再要人指定货架，
// 「指定某一个架」这件事全仓再无消费方）：收窄**只能**来自服务端按账号 scope 求，
// 不再有客户端可控的那一半。
//
// 收口规则（`prod::scan::listing::service::pickable_shelf_scope`，语义逐条对齐
// `auth::rbac::CurrentUser::can_access_shelf`）：
// - `shelf_wildcard == true` **或** 角色含 `MANAGER` ⇒ `None`（SQL 不加谓词，全集）
// - 否则 ⇒ `Some(shelf_ids)`，谓词 `sh.id = ANY($n)`（空数组 ⇒ 空集）
//
// 取行与 COUNT 两条 SQL 带**同形**谓词，否则「返回空列表但 total 仍是全厂数」。
//
// ## 造用户方式
// 复用仓库既有办法（`tests/production/worker_pool.rs` 的同款本地 helper，本文件
// 独享复制）：`t_user` 直插 + `t_user_role` 逐架插 `SHELF_ACCOUNT` scope 行。
//
// ## 三个 scope 形态各自怎么造（2026-10-04 review 第 1 轮补齐）
// | 形态 | 登录后 `shelf_ids` / `shelf_wildcard` | 造法 | 用例 |
// |---|---|---|---|
// | 逐架绑定 | `[架A, …]` / `false` | `Scope::Bound(vec![…])` | 场景 1 / 2 / 5 |
// | wildcard | `[]` / **`true`** | `Scope::Wildcard`（插 `scope_id IS NULL` 行） | 场景 3 |
// | 空 scope | `[]` / `false` | 绑定一张架、**登录前**把它停用 ⇒ 登录时该 `scope_id` 被过滤 | 场景 7 |
// | 非 PRODUCTION 绑定 | `[INSPECTION 架]` / `false` | 直接用 fixture 预置的 `fx_part_shelf` | 场景 8 |
//
// ⚠️ **空 scope 造不出「零角色行」**：那种账号登录直接被
// `iam::service::session` 挡掉（`roles.is_empty()` → 20606 NO_ROLE），且拿不到
// `SHELF_ACCOUNT` 角色、过不了本端点 `require_any_role`。所以 `Scope::Bound(&[])`
// 在 helper 里直接 panic 并指向正确造法（场景 7）—— 见 [`Scope`]。

/// 本文件新造账号共用的明文密码（与 `fixtures/part.sql` 内嵌哈希同款，cost=12）。
const NEW_USER_PASSWORD: &str = "changeme";

/// SHELF_ACCOUNT 账号的货架 scope 形态（2026-10-04 新增）。
///
/// 刻意收 enum 而不是 `&[i64]`：空切片的语义在两种构造下**正好相反** ——
/// `Bound(&[])` 造不出「空 scope」（需要一个有效角色行 + 一个登录时被过滤掉的
/// `scope_id`），而「零角色行」又过不了登录。历史上用 `&[]` 表示「无 scope 行 ⇒
/// wildcard」，与「空数组 ⇒ 空集」的安全语义**方向相反**，照 doc 抄一遍极易把两个
/// 用例写反。`Bound` 收空切片在此 panic 并指向正确造法。
enum Scope {
    /// 逐架绑定：每个 id 插一条 `scope_type='shelf' AND scope_id=<id>` 的角色行。
    Bound(Vec<i64>),
    /// 一行 `scope_id IS NULL` ⇒ 登录时 `shelf_wildcard = true` ⇒ 不加谓词、全集。
    ///
    /// ⚠️ 该行**产品 API 建不出来**：`iam::service::account::validate_role_scope`
    /// 对 `SHELF_ACCOUNT` 硬校验 `scope_id.is_some()`，故 `POST /iam/users/{id}/roles`
    /// 必返 `40001 VALIDATION`（HTTP 422）。`shelf_wildcard` 只有 fixture / 直插 SQL
    /// 能造 ⇒ 本用例锁的是「万一库里存在这种行，读侧不会把它误当空集」。
    Wildcard,
}

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
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user");
    id
}

/// 插一个 `t_user_role` 行（user_id + role + scope）。
///
/// `scope_id = None` 的 `SHELF_ACCOUNT` 行是 iam 侧判定 `shelf_wildcard = true`
/// 的唯一来源（`iam::service::session::resolve_roles_and_scope`：判据是
/// `role == ShelfAccount && scope_type == 'shelf' && scope_id IS NULL` 三者同时成立），
/// 故本文件用它造 wildcard 账号。**产品 API 建不出这种行**（见 [`Scope::Wildcard`]）。
async fn add_shelf_account_role(pool: &PgPool, user_id: i64, shelf_id: Option<i64>) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 'SHELF_ACCOUNT', 'shelf', $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(user_id)
    .bind(shelf_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");
    id
}

/// 造一个指定 scope 形态的 SHELF_ACCOUNT 账号并登录，返回 access token。
async fn login_shelf_account(
    pool: &PgPool,
    app: &axum::Router,
    username: &str,
    scope: Scope,
) -> String {
    let uid = insert_user_with_password(pool, username, NEW_USER_PASSWORD).await;
    match scope {
        Scope::Bound(shelves) => {
            assert!(
                !shelves.is_empty(),
                "Scope::Bound(&[]) 造不出「空 scope」：零角色行的账号登录即被拒（20606 \
                 NO_ROLE），且拿不到 SHELF_ACCOUNT 角色。空 scope 请用「绑定一张架后、\
                 登录前停用它」——见 pickable_scope_empty_when_bound_shelf_deactivated"
            );
            for sid in shelves {
                add_shelf_account_role(pool, uid, Some(sid)).await;
            }
        }
        Scope::Wildcard => {
            add_shelf_account_role(pool, uid, None).await;
        }
    }
    login_token(app, username, NEW_USER_PASSWORD).await
}

/// 软删一张货架（`t_shelf.deleted_at`）。列表侧补这个守卫后，其上的批次不再可见。
async fn soft_delete_shelf(pool: &PgPool, shelf_id: i64) {
    sqlx::query("UPDATE t_shelf SET deleted_at = now() WHERE id = $1")
        .bind(shelf_id)
        .execute(pool)
        .await
        .expect("soft delete t_shelf");
}

/// 停用一张货架（`is_active = false` + `deleted_at = now()`，与
/// `ShelfService::soft_delete_shelf` 的写侧同形）。
///
/// 本文件用它造**空 scope**：登录时 `resolve_roles_and_scope` 会校验被绑货架的
/// `is_active`（并经 `get_shelf_by_id` 过滤软删），非 active 的 `scope_id` 不进
/// `shelf_ids` ⇒ 登录后 `shelf_ids == []` 且 `shelf_wildcard == false`
/// （区别于 wildcard 的 `[]` + `true`）。
async fn deactivate_shelf(pool: &PgPool, shelf_id: i64) {
    sqlx::query("UPDATE t_shelf SET is_active = false, deleted_at = now() WHERE id = $1")
        .bind(shelf_id)
        .execute(pool)
        .await
        .expect("deactivate t_shelf");
}

/// 读回服务端为该 token 算出的 `shelf_ids`（`GET /iam/me`）。
///
/// 用途：scope 类用例必须先自证「服务端到底算出了什么 scope」，否则「列表为空」可能
/// 是另一条原因（架被停用 / scope 绑的是别的区）造成的，断言就失去回归价值。
/// ⚠️ 顺带覆盖 `GET /iam/me` 出参的一个缺口：响应里**没有** `shelf_wildcard` 键，
/// 所以只能断言 `shelf_ids`，wildcard 与空 scope 在这里同形。
async fn server_side_shelf_ids(app: &axum::Router, token: &str) -> Vec<String> {
    let (s, env) = send(
        app.clone(),
        json_request("GET", "/iam/me", None::<Value>, Some(token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET /iam/me: {env}");
    env["data"]["shelf_ids"]
        .as_array()
        .expect("data.shelf_ids 是数组")
        .iter()
        .map(|v| v.as_str().expect("shelf_ids 元素是 string").to_string())
        .collect()
}

/// 断言信封的 `items` 与 `total` 自洽（`total` 必须等于收窄后的可见行数）。
fn assert_total_matches_items(env: &Value, uri: &str) {
    let n = env["data"]["items"].as_array().expect("data.items").len();
    assert_eq!(
        env["data"]["total"], n as i64,
        "total 与 items 的 scope 谓词必须同形（否则分页总数会说谎）: {uri}: {env}"
    );
}

/// 场景 1（**核心**）：绑架 A 的 SHELF_ACCOUNT 只能看到架 A 上的批次。
///
/// 修复前：本端点不收口 ⇒ 两张架的批次都在响应里。
#[tokio::test]
async fn pickable_scope_closed_to_single_bound_shelf() {
    let (pool, app, _mgr_token, fx) = bootstrap().await;
    // 架 A = fixture 预置的 PRODUCTION 架；架 B = 本测试新造
    let shelf_a = PRODUCTION_SHELF_ID;
    let shelf_b = insert_production_shelf(&pool, "SCOPE-SH-B").await;
    let on_a = insert_part(&pool, fx.part_customer_l1_id, "架A件", "D-SCOPE-A", None).await;
    let on_b = insert_part(&pool, fx.part_customer_l1_id, "架B件", "D-SCOPE-B", None).await;
    insert_pickable_batch(&pool, on_a, shelf_a, fx.process_a_id, 1).await;
    insert_pickable_batch(&pool, on_b, shelf_b, fx.process_a_id, 1).await;

    let token = login_shelf_account(&pool, &app, "scope_single", Scope::Bound(vec![shelf_a])).await;
    let uri = format!("{PICKABLE_URI_PREFIX}?work_type_id={}", fx.work_type_a_id);
    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(
        part_ids(&env),
        vec![on_a.to_string()],
        "绑架 A 的账号不得看到架 B 的批次: {env}"
    );
    assert_total_matches_items(&env, &uri);
}

/// 场景 2：绑架 A + 架 B ⇒ 看到 **A ∪ B**（多架是并集不是交集也不是只取第一个）。
#[tokio::test]
async fn pickable_scope_is_union_of_bound_shelves() {
    let (pool, app, _mgr_token, fx) = bootstrap().await;
    let shelf_a = PRODUCTION_SHELF_ID;
    let shelf_b = insert_production_shelf(&pool, "SCOPE-SH-B").await;
    let shelf_c = insert_production_shelf(&pool, "SCOPE-SH-C").await;
    let on_a = insert_part(&pool, fx.part_customer_l1_id, "架A件", "D-SCOPE-A", None).await;
    let on_b = insert_part(&pool, fx.part_customer_l1_id, "架B件", "D-SCOPE-B", None).await;
    let on_c = insert_part(&pool, fx.part_customer_l1_id, "架C件", "D-SCOPE-C", None).await;
    insert_pickable_batch(&pool, on_a, shelf_a, fx.process_a_id, 1).await;
    insert_pickable_batch(&pool, on_b, shelf_b, fx.process_a_id, 1).await;
    insert_pickable_batch(&pool, on_c, shelf_c, fx.process_a_id, 1).await;

    let token = login_shelf_account(
        &pool,
        &app,
        "scope_union",
        Scope::Bound(vec![shelf_a, shelf_b]),
    )
    .await;
    let uri = format!("{PICKABLE_URI_PREFIX}?work_type_id={}", fx.work_type_a_id);
    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    let mut got = part_ids(&env);
    got.sort();
    let mut want = vec![on_a.to_string(), on_b.to_string()];
    want.sort();
    assert_eq!(
        got, want,
        "scope 必须是并集 A∪B（架 C 不在 scope 内）: {env}"
    );
    assert_total_matches_items(&env, &uri);
}

/// 场景 3：**wildcard**（SHELF_ACCOUNT 的 `scope_id = NULL`）⇒ 不受收口，可见全集。
#[tokio::test]
async fn pickable_scope_wildcard_sees_all_shelves() {
    let (pool, app, _mgr_token, fx) = bootstrap().await;
    let shelf_b = insert_production_shelf(&pool, "SCOPE-SH-B").await;
    let on_a = insert_part(&pool, fx.part_customer_l1_id, "架A件", "D-SCOPE-A", None).await;
    let on_b = insert_part(&pool, fx.part_customer_l1_id, "架B件", "D-SCOPE-B", None).await;
    insert_pickable_batch(&pool, on_a, PRODUCTION_SHELF_ID, fx.process_a_id, 1).await;
    insert_pickable_batch(&pool, on_b, shelf_b, fx.process_a_id, 1).await;

    let token = login_shelf_account(&pool, &app, "scope_wildcard", Scope::Wildcard).await;
    let uri = format!("{PICKABLE_URI_PREFIX}?work_type_id={}", fx.work_type_a_id);
    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    let mut got = part_ids(&env);
    got.sort();
    let mut want = vec![on_a.to_string(), on_b.to_string()];
    want.sort();
    assert_eq!(got, want, "wildcard 账号不受收口: {env}");
    assert_total_matches_items(&env, &uri);
}

/// 场景 4：**Manager** 角色 ⇒ 不受收口（`can_access_shelf` 对 Manager 短路 true）。
#[tokio::test]
async fn pickable_scope_manager_bypasses_closure() {
    let (pool, app, mgr_token, fx) = bootstrap().await;
    let shelf_b = insert_production_shelf(&pool, "SCOPE-SH-B").await;
    let on_a = insert_part(&pool, fx.part_customer_l1_id, "架A件", "D-SCOPE-A", None).await;
    let on_b = insert_part(&pool, fx.part_customer_l1_id, "架B件", "D-SCOPE-B", None).await;
    insert_pickable_batch(&pool, on_a, PRODUCTION_SHELF_ID, fx.process_a_id, 1).await;
    insert_pickable_batch(&pool, on_b, shelf_b, fx.process_a_id, 1).await;

    // bootstrap 的 token 就是 MANAGER（且无 SHELF_ACCOUNT 行 ⇒ 靠 Manager 角色短路）
    let uri = format!("{PICKABLE_URI_PREFIX}?work_type_id={}", fx.work_type_a_id);
    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&mgr_token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(part_ids(&env).len(), 2, "Manager 必须见全集: {env}");
    assert_total_matches_items(&env, &uri);
}

/// 场景 6（**行为变更**）：软删货架上的批次不再出现在结果里（取行 + COUNT 同步）。
///
/// 修复前 `JOIN t_shelf` 缺 `deleted_at IS NULL`，而 pick-up 写侧的
/// `validate_shelf_zone` 走 `ShelfRepo::get_by_id`（带软删守卫）会拒软删架 ⇒
/// 「列表给出但提交必被拒」。
#[tokio::test]
async fn pickable_excludes_batches_on_soft_deleted_shelf() {
    let (pool, app, mgr_token, fx) = bootstrap().await;
    let shelf_b = insert_production_shelf(&pool, "SCOPE-SH-B").await;
    let on_a = insert_part(&pool, fx.part_customer_l1_id, "架A件", "D-SCOPE-A", None).await;
    let on_b = insert_part(&pool, fx.part_customer_l1_id, "架B件", "D-SCOPE-B", None).await;
    insert_pickable_batch(&pool, on_a, PRODUCTION_SHELF_ID, fx.process_a_id, 1).await;
    insert_pickable_batch(&pool, on_b, shelf_b, fx.process_a_id, 1).await;
    // 前提自证：软删前两张架都可见
    let uri = format!("{PICKABLE_URI_PREFIX}?work_type_id={}", fx.work_type_a_id);
    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&mgr_token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(
        part_ids(&env).len(),
        2,
        "前提：软删前两张架的批次都在: {env}"
    );

    soft_delete_shelf(&pool, shelf_b).await;

    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&mgr_token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(
        part_ids(&env),
        vec![on_a.to_string()],
        "软删架上的批次不得再出现（与 pick-up 写侧一致）: {env}"
    );
    assert_total_matches_items(&env, &uri);
}

/// 场景 7（**空 scope 分支，2026-10-04 review 第 1 轮补**）：`shelf_ids == []` 且
/// `shelf_wildcard == false` ⇒ 必须返空集，**不是**「无限制」。
///
/// 这条是 `pickable_shelf_scope` 里被显式标为安全关键的分支：一旦有人把
/// `Some(vec![])` 误写成「空 ⇒ 不加谓词」，未绑架 / 已失去全部绑定架的 SHELF_ACCOUNT
/// 就会重新看到全厂 PRODUCTION 架。
///
/// **造法**（与 wildcard 的方向必须能一眼分辨）：
/// 1. 账号绑一张 PRODUCTION 架 `dead`（`scope_id = dead`）；
/// 2. **登录前**把 `dead` 停用 ⇒ 登录时 `resolve_roles_and_scope` 的
///    `s.is_active` 校验不过 ⇒ 该 `scope_id` 不进 `shelf_ids`；
/// 3. ⇒ 登录后 `shelf_ids == []` / `shelf_wildcard == false`（= 空 scope，
///    而 wildcard 是 `[]` + `true`，见场景 3）。
///
/// ⚠️ 停用的必须是**另一张**架：架 A 上的批次要保持 `is_active = true`，否则列表
/// 为空是「架被停用」造成的，这条用例就变成恒真断言、失去回归价值（下方先自证
/// Manager 能看到 1 条）。
#[tokio::test]
async fn pickable_scope_empty_when_bound_shelf_deactivated() {
    let (pool, app, mgr_token, fx) = bootstrap().await;
    // 架 A：放可领批次，全程 active；架 dead：只用来绑 scope，随后停用
    let dead = insert_production_shelf(&pool, "SCOPE-SH-DEAD").await;
    let on_a = insert_part(&pool, fx.part_customer_l1_id, "架A件", "D-SCOPE-A", None).await;
    insert_pickable_batch(&pool, on_a, PRODUCTION_SHELF_ID, fx.process_a_id, 1).await;

    let uri = format!("{PICKABLE_URI_PREFIX}?work_type_id={}", fx.work_type_a_id);
    // 前提自证：批次确实可领（否则下面的空集断言是恒真的）
    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&mgr_token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(
        part_ids(&env),
        vec![on_a.to_string()],
        "前提：架 A 上的批次可领（Manager 可见）: {env}"
    );

    // 绑定 dead 架 → 停用 dead 架 → 登录（登录时 scope 被过滤成空数组）
    let uid = insert_user_with_password(&pool, "scope_empty", NEW_USER_PASSWORD).await;
    add_shelf_account_role(&pool, uid, Some(dead)).await;
    deactivate_shelf(&pool, dead).await;
    let token = login_token(&app, "scope_empty", NEW_USER_PASSWORD).await;

    // 分支自证：服务端算出的必须是**空数组**（`[]` + 非 wildcard）。若它算出了
    // [dead] 之类，下面的空集断言就变成了「架被停用」而不是「空 scope」在起作用。
    let scope = server_side_shelf_ids(&app, &token).await;
    assert!(
        scope.is_empty(),
        "前提：停用后该账号登录的 shelf_ids 必须是空数组（空 scope 分支）: {scope:?}"
    );

    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert!(
        env["data"]["items"].as_array().expect("items").is_empty(),
        "空 scope（shelf_ids=[] 且非 wildcard）必须返空集：若这里看到架 A 的批次，\
         说明空数组被当成了「无限制」: {env}"
    );
    assert_eq!(
        env["data"]["total"], 0,
        "COUNT 必须带同一条 scope 谓词，空 scope 下同样是 0: {env}"
    );
    assert_total_matches_items(&env, &uri);
}

/// 场景 8（**2026-10-04 review 第 1 轮补**）：scope 非空但**全是 INSPECTION 架** ⇒
/// 与本端点的 `sh.zone = 'PRODUCTION'` 硬过滤求交为空。
///
/// 直接用 fixture 预置的 `fx_part_shelf`（`fixtures/part.sql` 里
/// `SHELF_ACCOUNT` + `scope_type='shelf'` + `scope_id=FX-SH-INSP`），不新造账号 ——
/// 这正是生产里「品检区一体机」的形状：登录后 `shelf_ids` **非空**（长度 1），
/// 与场景 7 的「空 scope」是两条不同分支（本例锁「非空但不含 PRODUCTION 架」）。
#[tokio::test]
async fn pickable_scope_bound_to_inspection_shelf_only_yields_nothing() {
    let (pool, app, mgr_token, fx) = bootstrap().await;
    let on_a = insert_part(&pool, fx.part_customer_l1_id, "架A件", "D-SCOPE-A", None).await;
    insert_pickable_batch(&pool, on_a, PRODUCTION_SHELF_ID, fx.process_a_id, 1).await;

    let uri = format!("{PICKABLE_URI_PREFIX}?work_type_id={}", fx.work_type_a_id);
    // 前提自证：批次可领
    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&mgr_token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(
        part_ids(&env).len(),
        1,
        "前提：架 A 上的批次可领（Manager 可见）: {env}"
    );

    // fixture 预置账号：SHELF_ACCOUNT scope 绑在 INSPECTION 架上（非空数组）
    let token = login_token(
        &app,
        &fx.part_shelf_account_username,
        ProductionFixture::PASSWORD,
    )
    .await;

    // 分支自证：scope 必须是**非空**的（长度 1 的 INSPECTION 架），与场景 7 的空数组
    // 是两条不同分支。
    let scope = server_side_shelf_ids(&app, &token).await;
    assert_eq!(
        scope,
        vec![PartFixture::INSPECTION_SHELF_ID.to_string()],
        "前提：fixture 账号的 shelf_ids 应恰是那一张 INSPECTION 架（非空 scope 分支）: {scope:?}"
    );

    let (s, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "GET {uri}: {env}");
    assert!(
        env["data"]["items"].as_array().expect("items").is_empty(),
        "scope 只含 INSPECTION 架时，与本端点的 PRODUCTION 架过滤求交为空（scope 非空，\
         与场景 7 的空数组是两条不同分支）: {env}"
    );
    assert_eq!(env["data"]["total"], 0, "COUNT 同样为 0: {env}");
    assert_total_matches_items(&env, &uri);
}

// ===========================================================================
//  2026-10-04（随端点迁入）：part 侧业务列投影真实值
// ===========================================================================
//
// 三个同族端点此前共享一个根因：取行 SQL 只投影 `p.id` / `p.serial_no` /
// `p.drawing_no` 三列，剩下的行字段靠**手抄 20 个占位值**的字面量填。于是 `name`
// 填成图号副本（前端卡片第 1 行与第 2 行重复）、`is_urgent` 恒 `false`（加急 tag
// 永不渲染）、`planned_delivery_date` 恒 `1970-01-01`。
//
// 而 pickable 列表的 `ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC` 排的
// 是 **DB 真实列** ⇒ 列表已经按加急排好了，工件上却看不出任何标记。
//
// 断言手法：造 part 时让 `name` **不等于** `drawing_no` —— 改造前 `name` 就是图号
// 副本，两者相等时任何 `assert_eq!(name, …)` 都测不出漂移。另外 `is_urgent` /
// `planned_delivery_date` 刻意取**非默认值**，否则「投影到真值」与「仍填占位」不可区分。
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
        "列表响应恒不含 next_process_id: {item}"
    );
    // 2026-10-10：占位字段已随 VO 收敛整体退场（见 `ScanListItem` 的 doc）。
    for gone in [
        "applicant_name",
        "request_date",
        "customer_id",
        "assembly_id",
        "status",
        "order_no",
        "note",
        "unit_price",
        "total_price",
        "version",
        "created_at",
        "created_by",
        "updated_at",
        "updated_by",
        "deleted_at",
        "customer_name",
        "l1_customer_name",
        "holder_name",
        "row_type",
        "has_children",
        "child_count",
        "has_cnc_program",
    ] {
        assert!(
            item.get(gone).is_none(),
            "`{gone}` 是恒为占位值的字段，2026-10-10 起不该再出现在行里: {item}"
        );
    }
}

/// `GET /prod/scan/pickable`：part 侧 4 列投影真实值。
#[tokio::test]
async fn pickable_projects_real_part_business_fields() {
    let (pool, app, token, fx) = bootstrap().await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "可领急件名",
        "D-BIZ-PICK",
        Some("BIZ-PICK-001"),
    )
    .await;
    sqlx::query(
        "UPDATE t_part SET is_urgent = true, \
           planned_delivery_date = DATE '2026-10-20', system_delivery_date = NULL \
         WHERE id = $1",
    )
    .bind(part_id)
    .execute(&pool)
    .await
    .expect("mark part urgent");
    insert_pickable_batch(&pool, part_id, PRODUCTION_SHELF_ID, fx.process_a_id, 2).await;

    let env = get_pickable(&app, &token, fx.work_type_a_id).await;
    let item = item_by_part_id(&env, part_id);
    // system_delivery_date 为 NULL ⇒ 序列化成 null（不是 1970-01-01）
    assert_real_part_fields(item, "可领急件名", true, "2026-10-20", None);
    assert_ne!(
        item["name"], item["drawing_no"],
        "前提自证：name 与 drawing_no 必须不同（相等时测不出「name 填成图号」的漂移）: {item}"
    );
}

/// `GET /prod/scan/held`：part 侧 4 列投影真实值。
#[tokio::test]
async fn held_projects_real_part_business_fields() {
    let (pool, app, token, fx) = bootstrap().await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "持有中工单名",
        "D-BIZ-HELD",
        Some("BIZ-HELD-001"),
    )
    .await;
    sqlx::query(
        "UPDATE t_part SET is_urgent = true, \
           planned_delivery_date = DATE '2027-01-15', \
           system_delivery_date = DATE '2026-12-20' \
         WHERE id = $1",
    )
    .bind(part_id)
    .execute(&pool)
    .await
    .expect("mark part urgent");
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-BIZ").await;
    insert_worker_held_batch(&pool, part_id, worker_id, 5).await;

    let item = by_worker_item(&app, &token, worker_id, part_id).await;
    assert_real_part_fields(
        &item,
        "持有中工单名",
        true,
        "2027-01-15",
        Some("2026-12-20"),
    );
}

// ===========================================================================
//  2026-10-10：`ScanListItem` 字段集 + 「哪些字段只有 held 有值」
// ===========================================================================

/// 行 VO 的键集合逐字钉死。
///
/// 后端**加字段**时前端 Zod 会静默 strip（报工台照常渲染、零报错，而那个字段永远
/// 没人消费）；**删字段**时前端必填声明会炸掉整份信封。这两种漂移只有把键集合
/// 写死才拦得住。
#[tokio::test]
async fn scan_list_item_key_set_is_pinned() {
    let (pool, app, token, fx) = bootstrap().await;
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "键集合用例件",
        "D-KEYSET",
        Some("KEYSET-001"),
    )
    .await;
    insert_pickable_batch(&pool, part_id, PRODUCTION_SHELF_ID, fx.process_a_id, 2).await;

    let env = get_pickable(&app, &token, fx.work_type_a_id).await;
    let item = item_by_part_id(&env, part_id);
    let mut keys: Vec<&str> = item
        .as_object()
        .expect("行应是 JSON 对象")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "batch_id",
            "batch_version",
            "chain_current_process_name",
            "chain_next_process_id",
            "chain_next_process_name",
            "chain_state",
            "drawing_no",
            "has_process_chain",
            "id",
            "is_urgent",
            "location",
            "name",
            "planned_delivery_date",
            "process_chain_id",
            "quantity",
            "serial_no",
            "system_delivery_date",
        ],
        "ScanListItem 的字段集变了：加字段要同步前端 scanPartRowSchema，\
         删字段要先确认前端无消费方并同步改本断言"
    );
    // `location` 值恒 null，但**键必须在** —— 前端 `BatchPickerDialog.holderText`
    // 用「键存在性」判断要不要渲染 holder 行，键被 strip 掉时卡片静默少一行。
    assert!(
        item.as_object().unwrap().contains_key("location"),
        "location 键必须在（哪怕值恒 null）: {item}"
    );
    assert!(item["location"].is_null(), "location 值恒 null: {item}");
    // 序列化方向：雪花 id 一律 string，计数裸数字
    assert!(item["id"].is_string(), "id 必须是 JSON string: {item}");
    assert_eq!(item["batch_version"], 2, "batch_version 是裸数字: {item}");
    assert!(item["quantity"].is_number(), "quantity 是裸数字: {item}");
}

/// **同一个批次**在两条端点上的链字段：pickable 侧是占位、held 侧是真值。
///
/// 这是本 VO 最容易被误读的一处：链四字段 + `process_chain_id` 只在 `held` 侧填，
/// 因为链位置是**批次级**事实，而 `pickable` 侧那行「还没被领走」，前端不需要知道
/// 它的下一道工序（放回分流只在放回页做，而放回页读的是 `held`）。取保守默认
/// （`NONE` / `"0"` / `null` / `null`）而不是编一个值，是因为 `NONE` 的语义与
/// 「不知道」同向 —— 宁可让用户手填，也不给一个可能错的下一道。
#[tokio::test]
async fn chain_fields_only_held_side_has_values() {
    let (pool, app, token, fx) = bootstrap().await;
    // 链 = [A(sort_order 10), B(20)]；批次当前工序 = A（指向 step_a）
    // ⇒ 链位置 NEXT、下一道 = B。
    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "链字段对照件",
        "D-CHAIN-PAIR",
        Some("CHAIN-PAIR-1"),
    )
    .await;
    let chain_id = create_chain(&pool, "chain-pair").await;
    bind_part_to_chain(&pool, part_id, chain_id).await;
    let step_a = add_chain_step(&pool, chain_id, fx.process_a_id, 10).await;
    add_chain_step(&pool, chain_id, fx.process_b_id, 20).await;

    // ---- 同一个批次，先在生产架上（pickable 侧），再交给工人（held 侧）----
    // current_process 必须取 `process_a`：`t_work_type_process` 里只有 FX-WTA ↔
    // FX-PROC-A 的映射，取 B 的话 pickable 根本看不见这行（与链位置无关）。
    let pick_batch =
        insert_pickable_batch(&pool, part_id, PRODUCTION_SHELF_ID, fx.process_a_id, 3).await;
    set_batch_position(&pool, pick_batch, Some(fx.process_a_id), Some(step_a)).await;

    let pick_env = get_pickable(&app, &token, fx.work_type_a_id).await;
    let pick_item = item_by_part_id(&pick_env, part_id);
    assert_none_state(pick_item);
    assert!(
        pick_item["process_chain_id"].is_null(),
        "pickable 侧不投影 process_chain_id（恒 null）: {pick_item}"
    );
    assert!(
        pick_item["has_process_chain"].as_bool().is_some(),
        "has_process_chain 两条端点都填: {pick_item}"
    );

    // 交给工人
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WK-CHAIN-PAIR").await;
    sqlx::query(
        "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = $2 \
         WHERE id = $1",
    )
    .bind(pick_batch)
    .bind(worker_id)
    .execute(&pool)
    .await
    .expect("hand the batch to the worker");

    let held_item = by_worker_item(&app, &token, worker_id, part_id).await;
    assert_eq!(
        held_item["chain_state"], "NEXT",
        "held 侧填真值: {held_item}"
    );
    assert_eq!(
        held_item["chain_next_process_id"].as_str(),
        Some(fx.process_b_id.to_string().as_str()),
        "held 侧的下一道是链上第二道（工序 B）: {held_item}"
    );
    assert_eq!(
        held_item["chain_next_process_name"], "FX 工序 NB",
        "held 侧的下一道工序名（字面值取自 test-support/fixtures/production.sql \
         的 FX-NB 行）: {held_item}"
    );
    assert_eq!(
        held_item["chain_current_process_name"], "FX 工序 NA",
        "held 侧的当前工序名: {held_item}"
    );
    assert_eq!(
        held_item["process_chain_id"].as_str(),
        Some(chain_id.to_string().as_str()),
        "held 侧投影真实链 id: {held_item}"
    );
}

// ===========================================================================
//  2026-10-10：旧路径全部失效（硬切无 alias）
// ===========================================================================

/// 5 条旧端点逐一断言已失效。
///
/// ⚠️ 响应码**按实跑结果**写。实跑结论与直觉相反，值得记一笔：
/// **5 条全是 404，没有一条是 400**。
/// - 两条 `/parts/...` 的直觉依据是「落进 `/{part_id}` catch-all ⇒ `Path<i64>`
///   拒绝非数字段 ⇒ 400」。**不成立**：part 域的 2 段路由（`/{part_id}/update` 等）
///   第二段是**字面量**（`update` / `cancel` / `pick-up` …），`matchit` 匹配不上
///   `1` ⇒ 根本没有路由命中 ⇒ 干净的 404。`Path` 提取器压根没机会拒绝。
/// - `POST /prod/workers/verify-badge`：落进 worker 域的 `/{id}`，那条**只注册了
///   GET** ⇒ 方法不匹配返 405（该条由 `scan_badge.rs` 守）。
/// - 两条 `/prod/batches/...`：整个路由段已不存在 ⇒ 404。
///
/// 结论：**跨域硬切时不要预设响应码**。`/shelves/*` 是干净 404、`/iam/shelves/*`
/// 落 catch-all 才是 400 —— 差别在「被 catch-all 吃掉的那条路径有没有字面量第二段」。
#[tokio::test]
async fn old_scan_paths_are_gone() {
    let (_pool, app, token, _fx) = bootstrap().await;

    for (uri, method, want) in [
        (
            "/parts/pickable-by-work-type/1",
            "GET",
            StatusCode::NOT_FOUND,
        ),
        ("/parts/by-worker/1", "GET", StatusCode::NOT_FOUND),
        (
            "/prod/workers/verify-badge",
            "POST",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        ("/prod/batches/worker-scan", "POST", StatusCode::NOT_FOUND),
        ("/prod/batches/1/pick-up", "POST", StatusCode::NOT_FOUND),
    ] {
        let body = if method == "POST" {
            Some(json!({"serial_no": "X", "badge_code": "X"}))
        } else {
            None
        };
        // 走 `send_raw`：400 / 405 / 404 的响应体是纯文本（axum 提取器 rejection /
        // 方法不匹配 / 空路由），不走 `R<T>` 信封，`send` 会在 JSON 解析处 panic ——
        // 而那正是本用例要断言的形态。
        let (status, raw) =
            send_raw(app.clone(), json_request(method, uri, body, Some(&token))).await;
        assert_eq!(
            status, want,
            "旧路径 {method} {uri} 应当失效，实得 {status}: {raw}"
        );
    }
}
