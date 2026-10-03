//! `GET /parts/pickable-by-work-type/{work_type_id}` 出参集成测试（2026-10-03 新增）
//!
//! 本端点此前在 `tests/` 下**零覆盖**（只在 `to_inspection.rs` 的注释里被提到）。
//! 2026-10-03 起本端点是扫码台「领料」的字段级契约载体，故在此补最小覆盖：
//! `PartListItem` 新增的 `batch_id` / `batch_version` 必须等于该行
//! `t_part_batch.id` / `t_part_batch.version`。
//!
//! ## 过滤条件（全套都满足，行才进列表）
//! 批次 `status='IN_PROCESS'` + `location='PRODUCTION_SHELF'` + `deleted_at IS NULL`，
//! 挂在 `zone='PRODUCTION'` 且 `is_active=true` 的货架上（`t_shelf_process` 不参与
//! 本端点过滤），且批次 `current_process_id` 命中 `t_work_type_process` 里该工种的
//! **活跃**映射；另需 part 本身 `deleted_at IS NULL`。
//!
//! fixture 复用 `ProductionFixture`（它内部先 `load_part_fixture`，故 `FX-PROC-A`
//! 工序 / `FX-WT-A` 工种 / `FX-WTA ↔ FX-PROC-A` 映射都是预置的）。生产货架取
//! `PartFixture::PRODUCTION_SHELF_ID` 常量（`ProductionFixture` 只保留 part 域基线的
//! 几个 id，没转出 production shelf）。本文件只需直插 part / batch / 附加货架。

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_test_support::fixture::{PartFixture, ProductionFixture};
use hsh_erp_test_support::{
    json_request, load_production_fixture, login_token, pool_snowflake, send, test_app, test_pool,
    test_state,
};

/// 可领取列表路径前缀（测试 app 不带 `/api/v2`）。
const PICKABLE_URI_PREFIX: &str = "/parts/pickable-by-work-type";

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
    let uri = format!("{PICKABLE_URI_PREFIX}/{work_type_id}");
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
    // part 级 `version` 在本端点恒为 0（取行 SQL 不投影 `p.version`）——批次 OCC 只认
    // `batch_version`，两个字段不可混用。
    assert_eq!(
        item["version"], 0,
        "part 级 version 仍是 0 占位（与 batch_version 严格区分）: {env}"
    );
    // 行内其余字段仍由手工 TPart 占位派生：quantity 取自批次、drawing_no 来自 part
    assert_eq!(item["quantity"], 2, "quantity 取自批次而非 part: {env}");
    assert_eq!(item["drawing_no"], "D-PICK-A", "{env}");
}

/// `shelf_id` 过滤：只返指定货架上的批次，但 `batch_id` 仍是各自那一行的批次。
#[tokio::test]
async fn pickable_shelf_filter_keeps_per_batch_anchor() {
    let (pool, app, token, fx) = bootstrap().await;
    let on_fixture_shelf = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "可领件B",
        "D-PICK-B",
        Some("PICK-B-001"),
    )
    .await;
    let batch_b = insert_pickable_batch(
        &pool,
        on_fixture_shelf,
        PRODUCTION_SHELF_ID,
        fx.process_a_id,
        2,
    )
    .await;

    // 另造一张 PRODUCTION 货架 + 其上的可领批次，验证 shelf_id 过滤
    let other_shelf = insert_production_shelf(&pool, "PICK-SH-OTHER").await;
    let on_other_shelf = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "可领件C",
        "D-PICK-C",
        Some("PICK-C-001"),
    )
    .await;
    let batch_c =
        insert_pickable_batch(&pool, on_other_shelf, other_shelf, fx.process_a_id, 6).await;

    // 不带 shelf_id → 两张货架上的批次都在，且 batch_id 各自不同（证明不是「整表同一个 id」）
    let env = get_pickable(&app, &token, fx.work_type_a_id).await;
    assert_eq!(
        item_by_part_id(&env, on_fixture_shelf)["batch_id"].as_str(),
        Some(batch_b.to_string().as_str()),
        "batch_id 必须是该行自己的批次: {env}"
    );
    assert_eq!(
        item_by_part_id(&env, on_other_shelf)["batch_id"].as_str(),
        Some(batch_c.to_string().as_str()),
        "batch_id 必须是该行自己的批次: {env}"
    );
    assert_eq!(
        item_by_part_id(&env, on_other_shelf)["batch_version"],
        6,
        "batch_version 同样逐行对应: {env}"
    );

    // 指定 shelf_id → 只剩该货架上的行
    let uri = format!(
        "{PICKABLE_URI_PREFIX}/{}?shelf_id={PRODUCTION_SHELF_ID}",
        fx.work_type_a_id
    );
    let (status, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(
        part_ids(&env),
        vec![on_fixture_shelf.to_string()],
        "shelf_id 过滤后只剩该货架上的行: {env}"
    );
    assert_eq!(
        env["data"]["items"][0]["batch_id"].as_str(),
        Some(batch_b.to_string().as_str()),
        "shelf_id 过滤不影响 batch_id: {env}"
    );
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
//   · `list_pickable_by_work_type`（`GET /parts/pickable-by-work-type/{id}`）
//   · `list_by_work_type`（`GET /parts/by-work-type/{id}`）  ← 本节用例 1
//   · `list_by_worker`（`GET /parts/by-worker/{id}`）        ← 本节用例 2

/// 插一个 active 且已绑 work_type 的工人（`by-work-type` 走 `t_worker` JOIN，
/// `by-worker` 以 worker_id 为过滤锚点）。
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
/// 这是 `by-work-type` / `by-worker` 两个端点的硬过滤条件。
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

/// 2026-10-03 review 第 1 轮 Major-2 用例 1：`GET /parts/by-work-type/{id}` 遇
/// `serial_no IS NULL` 的手工工单必须 200 且该行 `serial_no` 为 `null`。
///
/// 修复前本端点整页 500（`by-work-type` 是 `pickable-by-work-type` 的同族兄弟，
/// 扫码台两个列表页挨着）。用例刻意造 `serial_no: None` 的行。
#[tokio::test]
async fn by_work_type_tolerates_null_serial_no() {
    let (pool, app, token, fx) = bootstrap().await;
    let worker_id = insert_active_worker(&pool, fx.work_type_a_id, "WT-SERIAL-NULL").await;
    let manual_part = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "无序列号件",
        "D-SERIAL-NULL",
        None,
    )
    .await;
    let _manual_batch = insert_worker_held_batch(&pool, manual_part, worker_id, 3).await;

    // 同工种再放一个**有**序列号的行：证明 null 行不是「整页空」而是「与有值行共存」
    let normal_part = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "有序列号件",
        "D-SERIAL-OK",
        Some("WT-OK-001"),
    )
    .await;
    let _normal_batch = insert_worker_held_batch(&pool, normal_part, worker_id, 5).await;

    let uri = format!("/parts/by-work-type/{}", fx.work_type_a_id);
    let (status, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "serial_no=NULL 不应 500: {env}");
    assert_eq!(env["code"], 0, "{uri}: {env}");

    let ids = part_ids(&env);
    assert_eq!(
        ids.len(),
        2,
        "null 行与有值行都应上榜（修复前整页 500、一行都拿不到）: {env}"
    );
    let null_item = item_by_part_id(&env, manual_part);
    assert!(
        null_item["serial_no"].is_null(),
        "手工工单的 serial_no 应序列化为 null 而非整页炸掉: {env}"
    );
    assert_eq!(
        item_by_part_id(&env, normal_part)["serial_no"],
        "WT-OK-001",
        "同页有序列号的行不受影响: {env}"
    );
}

/// 2026-10-03 review 第 1 轮 Major-2 用例 2：`GET /parts/by-worker/{id}` 同款。
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

    let uri = format!("/parts/by-worker/{worker_id}");
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
//  2026-10-04：`GET /parts/by-worker/{worker_id}` 的工序链派生
// ===========================================================================
//
// 报工台「放回」页要判定三态，全部依赖本端点行内字段（前端无第二数据源）：
//
// | `chain_state` | 含义                                   | 前端动作                        |
// |---------------|----------------------------------------|---------------------------------|
// | `NONE`        | 无链 / 链已软删 / 当前工序不在链内       | 弹工序选择框，让用户手填        |
// | `NEXT`        | 当前工序在链内且有下一道                | 免填，确认后直接放回            |
// | `TAIL`        | 当前工序是链内最后一道                  | 提示「加工完成后请送检」        |
//
// 本节同时锁住批次锚点（`batch_id` / `batch_version`）—— 放回页要发写请求，
// 拿不到批次 id 就发不出去。
//
// ## 链数据怎么造
// `ProcessChainFixture` 只预置「2 工序 + 2 part + 2 用户」，**不含任何链 / step 行**
// （见 `test-support/src/fixture/process_chain.rs` 的「当前域」），故链数据在本节
// 按 DB 约定现场直插：`t_part_process_chain` + `t_process_chain_step`（`sort_order`
// 稀疏 10/20/30，无物理外键，软删列留 NULL = 活跃）。
//
// ## 关键回归点
// 最后一则 `by_worker_chain_state_repositions_by_current_process_id_not_step_pointer`
// 锁的是「不能拿 step 指针的 `sort_order` 当位置」：worker-scan 的 RETURNED 只写
// `current_process_id`、不推进 `current_process_step_id`，多工序链批次第 2 次放回
// 时指针仍停在**首次定位**那一步，按位置推进会把当前工序自己当成下一道返回。

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

/// 打一次 by-worker 列表并按 part id 取出目标行（找不到即 panic 并打印整份信封）。
async fn by_worker_item(app: &axum::Router, token: &str, worker_id: i64, part_id: i64) -> Value {
    let uri = format!("/parts/by-worker/{worker_id}");
    let (status, env) = send(
        app.clone(),
        json_request("GET", &uri, None::<Value>, Some(token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(env["code"], 0, "GET {uri}: {env}");
    item_by_part_id(&env, part_id).clone()
}

/// 断言「取行 SQL 投影的 `b.id` / `b.version`」已填进出参（2026-10-04 之前恒 null）。
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
    assert_eq!(
        item["version"], 0,
        "part 级 version 仍是 0 占位，批次 OCC 只认 batch_version: {item}"
    );
}

/// 场景 1（`NONE` · 无链）：`p.process_chain_id IS NULL` ⇒ 前端弹工序选择框。
///
/// 同时锁住批次锚点与 `process_chain_id` 投影：三个断言都是 2026-10-04 之前
/// 恒为「占位值 / null」的字段。
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
    assert_eq!(item["chain_state"], "NONE", "无链 ⇒ NONE: {item}");
    assert_eq!(
        item["chain_next_process_id"].as_str(),
        Some("0"),
        "无链时下一道工序是 0 兜底（JSON string \"0\"，不是 null）: {item}"
    );
    assert!(
        item["chain_next_process_name"].is_null(),
        "NONE 时下一道工序名恒 null: {item}"
    );
    assert!(
        item["chain_current_process_name"].is_null(),
        "链内定位不成立时当前工序名也解析不出: {item}"
    );
    assert!(
        item["process_chain_id"].is_null(),
        "process_chain_id 现在取真实投影值（无链 ⇒ null）: {item}"
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
    assert_eq!(
        item["chain_state"], "NONE",
        "当前工序不在链内 ⇒ NONE（不能退化成按 step 指针位置给下一道）: {item}"
    );
    assert_eq!(
        item["chain_next_process_id"].as_str(),
        Some("0"),
        "NONE 时下一道工序是 0 兜底: {item}"
    );
    assert!(
        item["chain_next_process_name"].is_null(),
        "NONE 时下一道工序名恒 null: {item}"
    );
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
