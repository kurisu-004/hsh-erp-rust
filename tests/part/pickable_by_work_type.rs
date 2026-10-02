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
