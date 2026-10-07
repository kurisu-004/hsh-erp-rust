//! `POST /api/v2/com/delivery/note/{id}/driver`（指定司机）+ `/pickup`（领取）端到端测试
//!
//! 覆盖
//! 1. `validate_driver` 的 5 条判据（21409）：worker 行不存在 / 已软删 ·
//!    `is_active == false` · work_type 行取不到 · `work_type.code != "送货司机"` ·
//!    `work_type_id IS NULL`
//! 2. `POST /{id}/driver`：note 存在 + version 一致（40901）+ 指定成功后
//!    `driver_worker_name` 有值、`driver_worker_id` 已从 VO 移除（前端读姓名判空）
//! 3. `/pickup` 入参**不再有 `driver_worker_id`**：司机从单据上已指定的值读；
//!    未指定 ⇒ 21409
//! 4. `/pickup` **重跑** `validate_driver`：指定之后司机被停用 / 改工种 ⇒ 21409
//! 5. 正常领取：全部批次 READY_TO_SHIP ⇒ `PICKED_UP`
//! 6. 权限：Manager / Clerk / Inspector 可指定司机

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    DeliveryFixture, PartFixture, json_request, load_delivery_fixture, load_part_fixture,
    login_token, pool_snowflake, send, test_app, test_pool, test_state,
};

use hsh_erp_rust::infra::clock::now_naive;

/// 取一个测试用雪花 ID。
///
/// 2026-10-08 review 第 1 轮 B3：**必须**走 `test-support::pool_snowflake()`
/// （进程级 `OnceLock<Mutex<..>>`，instance 由 pid ⊕ 启动纳秒派生），不能每次
/// `SnowflakeIdGenerator::new(...)` 新建 —— 新建会把 `last_ms` / `sequence` 归零，
/// 同一毫秒内两次调用返回**完全相同**的 id（epoch 与 instance 都写死、seq 都从 0
/// 开始），撞 `t_*_pkey` 报 23505。共享一个生成器后同进程内 `next_id()` 串行发号，
/// 跨进程靠派生 instance 区分。
///
/// 这也顺带解掉了**跨文件**碰撞：同一 binary（`tests/com/main.rs`）里本文件与
/// `note.rs` / `group.rs` / `union_list.rs` 曾经各自 `new(..., 1)`，首个 id 相同。
/// 2026-10-08 起 `union_list.rs` 也改走了 `pool_snowflake()`，与本 helper 同一路径。
fn next_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
}

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token)
}

async fn insert_l1(pool: &PgPool, name: &str) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, NULL, 0, $3, NULL, $3, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(l1_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

/// 建一个 `SUBMITTED` 且挂了一个 `READY_TO_SHIP` 批次的单（领取用例的前置状态）。
/// 返回 `(note_id, version)`。
async fn insert_submitted_note_with_batch(pool: &PgPool, l1_id: i64) -> (i64, i32) {
    let note_id = next_id();
    let part_id = next_id();
    let batch_id = next_id();
    let serial = format!("DRV-{:010}", (part_id % 10_000_000_000) as u32);
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, '司机关联件', 'D-DRV', $3, 'READY_TO_SHIP', '司机测试', $4, $4, \
         4, 0, $5, NULL, $5, NULL)",
    )
    .bind(part_id)
    .bind(Some(serial))
    .bind(l1_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");

    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         delivery_note_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, 4, 'READY_TO_SHIP', 'PRODUCTION_SHELF', $3, 0, $4, NULL, $4, NULL)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(note_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");

    sqlx::query(
        "INSERT INTO t_delivery_note (id, delivery_note_no, customer_id, delivery_date, \
         status, submitted_at, submitted_by, version, created_at, created_by, updated_at, \
         updated_by) \
         VALUES ($1, $2, $3, $4, 'SUBMITTED', $5, NULL, 0, $5, NULL, $5, NULL)",
    )
    .bind(note_id)
    .bind(format!("DN-DRV-{:05}", note_id % 100_000))
    .bind(l1_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert submitted note");

    (note_id, 0)
}

async fn work_type(pool: &PgPool, code: &str, name: &str) -> i64 {
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM t_work_type WHERE code = $1")
        .bind(code)
        .fetch_optional(pool)
        .await
        .expect("query work_type")
    {
        return id;
    }
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_work_type (id, code, name, sort_order, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 999, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now_naive())
    .execute(pool)
    .await
    .expect("insert work_type");
    id
}

async fn insert_worker(
    pool: &PgPool,
    badge: &str,
    name: &str,
    wt_id: Option<i64>,
    active: bool,
    soft_deleted: bool,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, created_by, updated_at, updated_by, deleted_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, NULL, $6, NULL, $7)",
    )
    .bind(id)
    .bind(badge)
    .bind(name)
    .bind(active)
    .bind(wt_id)
    .bind(now)
    .bind(if soft_deleted { Some(now) } else { None })
    .execute(pool)
    .await
    .expect("insert worker");
    id
}

async fn set_driver(
    app: &axum::Router,
    token: &str,
    note_id: i64,
    driver_worker_id: i64,
    version: i32,
) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/note/{note_id}/driver"),
            Some(json!({"version": version, "driver_worker_id": driver_worker_id.to_string()})),
            Some(token),
        ),
    )
    .await
}

async fn pickup(
    app: &axum::Router,
    token: &str,
    note_id: i64,
    version: i32,
) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/note/{note_id}/pickup"),
            // ⚠️ 2026-10-08：入参**不再有** `driver_worker_id`
            Some(json!({"version": version})),
            Some(token),
        ),
    )
    .await
}

// ===========================================================================
//  1~2. /driver
// ===========================================================================

/// 指定司机成功：出参带 `driver_worker_name`，且**不带** `driver_worker_id`
/// （前端用姓名判空即可）。
#[tokio::test]
async fn set_driver_assigns_worker_and_returns_name_only() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "司机指定").await;
    let _l2 = insert_l2(&pool, "司机指定二厂", l1).await;
    let (note_id, v) = insert_submitted_note_with_batch(&pool, l1).await;
    let wt = work_type(&pool, "送货司机", "送货司机").await;
    let driver = insert_worker(&pool, "DR01", "戊司机", Some(wt), true, false).await;

    let (s, env) = set_driver(&app, &token, note_id, driver, v).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["driver_worker_name"], "戊司机");
    assert!(
        env["data"].get("driver_worker_id").is_none(),
        "2026-10-08：`driver_worker_id` 已从 VO 删除（前端读姓名判空）: {env}"
    );
    assert_eq!(env["data"]["version"], v + 1, "version++");

    // 落库校验
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT driver_worker_id FROM t_delivery_note WHERE id = $1")
            .bind(note_id)
            .fetch_one(&pool)
            .await
            .expect("read note");
    assert_eq!(stored, Some(driver));
}

/// `validate_driver` 的 5 条判据逐条钉住（全部 21409）。
///
/// ⚠️ 6 个子场景（含「worker 行不存在」）都作用在同一张**真实存在**的单上：校验链是
/// 「note 存在 → version 一致 → `validate_driver`」，note 不存在时报的是 21401。
#[tokio::test]
async fn validate_driver_rejects_all_five_failure_modes() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "司机闸门").await;
    let _l2 = insert_l2(&pool, "司机闸门二厂", l1).await;
    let driver_wt = work_type(&pool, "送货司机", "送货司机").await;
    let other_wt = work_type(&pool, "钳工", "钳工").await;

    // 全部 5 条判据都作用在**同一张真实存在的单**上（note 存在先于司机校验，
    // 否则报的是 21401 而不是 21409）。
    let (note_id, v0) = insert_submitted_note_with_batch(&pool, l1).await;

    // ① worker 行不存在
    let (_s1, env1) = set_driver(&app, &token, note_id, 9_999_999_999, v0).await;
    assert_eq!(env1["code"], 21409, "worker 不存在: {env1}");

    // ② 已软删
    let soft = insert_worker(&pool, "DR11", "已删司机", Some(driver_wt), true, true).await;
    let (_s2, env2) = set_driver(&app, &token, note_id, soft, v0).await;
    assert_eq!(env2["code"], 21409, "软删: {env2}");

    // ③ `is_active == false`
    let inactive = insert_worker(&pool, "DR12", "停用司机", Some(driver_wt), false, false).await;
    let (_s3, env3) = set_driver(&app, &token, note_id, inactive, v0).await;
    assert_eq!(env3["code"], 21409, "停用: {env3}");

    // ④ work_type 取不到（工种软删 → 悬空）
    let dangling_wt = work_type(&pool, "送货司机悬空", "送货司机悬空").await;
    let dangling = insert_worker(&pool, "DR13", "悬空司机", Some(dangling_wt), true, false).await;
    sqlx::query("UPDATE t_work_type SET deleted_at = now() WHERE id = $1")
        .bind(dangling_wt)
        .execute(&pool)
        .await
        .expect("soft delete work_type");
    let (_s4, env4) = set_driver(&app, &token, note_id, dangling, v0).await;
    assert_eq!(env4["code"], 21409, "工种软删: {env4}");

    // ⑤ `work_type.code != "送货司机"`
    let wrong = insert_worker(&pool, "DR14", "钳工阿离", Some(other_wt), true, false).await;
    let (_s5, env5) = set_driver(&app, &token, note_id, wrong, v0).await;
    assert_eq!(env5["code"], 21409, "非送货司机: {env5}");

    // ⑥ `work_type_id IS NULL`
    let no_wt = insert_worker(&pool, "DR15", "无工种人", None, true, false).await;
    let (_s6, env6) = set_driver(&app, &token, note_id, no_wt, v0).await;
    assert_eq!(env6["code"], 21409, "无工种: {env6}");

    // 单据未被写入任何司机
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT driver_worker_id FROM t_delivery_note WHERE id = $1")
            .bind(note_id)
            .fetch_one(&pool)
            .await
            .expect("read note");
    assert_eq!(stored, None, "6 次拒绝后单据必须仍是未指定司机");
}

/// version 不一致 ⇒ 40901，且不写司机。
#[tokio::test]
async fn set_driver_version_mismatch_returns_40901() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "司机 OCC").await;
    let _l2 = insert_l2(&pool, "司机 OCC 二厂", l1).await;
    let (note_id, _v) = insert_submitted_note_with_batch(&pool, l1).await;
    let wt = work_type(&pool, "送货司机", "送货司机").await;
    let driver = insert_worker(&pool, "DR21", "己司机", Some(wt), true, false).await;

    let (s, env) = set_driver(&app, &token, note_id, driver, 999).await;
    assert_eq!(s, StatusCode::CONFLICT, "{env}");
    assert_eq!(env["code"], 40901);
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT driver_worker_id FROM t_delivery_note WHERE id = $1")
            .bind(note_id)
            .fetch_one(&pool)
            .await
            .expect("read note");
    assert_eq!(stored, None, "OCC 失败必须零写入");
}

/// note 不存在 ⇒ 21401。
#[tokio::test]
async fn set_driver_on_missing_note_returns_21401() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let wt = work_type(&pool, "送货司机", "送货司机").await;
    let driver = insert_worker(&pool, "DR31", "庚司机", Some(wt), true, false).await;
    let (s, env) = set_driver(&app, &token, 9_999_999_999, driver, 0).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 21401);
}

// ===========================================================================
//  3~5. /pickup
// ===========================================================================

/// 正常领取：已指定司机 ⇒ `PICKED_UP`，批次翻 `DELIVERED`。
#[tokio::test]
async fn pickup_uses_assigned_driver_and_flips_batches_to_delivered() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "司机领取").await;
    let _l2 = insert_l2(&pool, "司机领取二厂", l1).await;
    let (note_id, v0) = insert_submitted_note_with_batch(&pool, l1).await;
    let wt = work_type(&pool, "送货司机", "送货司机").await;
    let driver = insert_worker(&pool, "DR41", "辛司机", Some(wt), true, false).await;

    let (_, env1) = set_driver(&app, &token, note_id, driver, v0).await;
    let v1 = env1["data"]["version"].as_i64().unwrap() as i32;

    let (s, env) = pickup(&app, &token, note_id, v1).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["status"], "PICKED_UP");
    assert_eq!(env["data"]["driver_worker_name"], "辛司机");

    let batch_status: String =
        sqlx::query_scalar("SELECT status FROM t_part_batch WHERE delivery_note_id = $1 LIMIT 1")
            .bind(note_id)
            .fetch_one(&pool)
            .await
            .expect("read batch status");
    assert_eq!(batch_status, "DELIVERED");
}

/// 未指定司机 ⇒ 21409（领取的前提是「已有合法司机」）。
///
/// 2026-10-08 起司机不再由 `POST /pickup` 的请求体传入，因此「忘了指定」必须由
/// 服务端从单据上发现，而不是等一个不存在的入参。
#[tokio::test]
async fn pickup_without_assigned_driver_returns_21409() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "未指定领取").await;
    let _l2 = insert_l2(&pool, "未指定领取二厂", l1).await;
    let (note_id, v) = insert_submitted_note_with_batch(&pool, l1).await;

    let (s, env) = pickup(&app, &token, note_id, v).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21409);
    let status: String = sqlx::query_scalar("SELECT status FROM t_delivery_note WHERE id = $1")
        .bind(note_id)
        .fetch_one(&pool)
        .await
        .expect("read note");
    assert_eq!(status, "SUBMITTED", "拒绝后单据不变");
}

/// ★ `/pickup` **重跑** `validate_driver`：指定之后司机被停用 ⇒ 21409。
///
/// 这是入参瘦身的直接后果：司机值来自单据而非请求体，可能在「指定 → 打印 → 领取」的
/// 窗口里失效，指定时校验过一次**不够**。
#[tokio::test]
async fn pickup_revalidates_driver_who_became_inactive_after_assignment() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "领取重校").await;
    let _l2 = insert_l2(&pool, "领取重校二厂", l1).await;
    let (note_id, v0) = insert_submitted_note_with_batch(&pool, l1).await;
    let wt = work_type(&pool, "送货司机", "送货司机").await;
    let driver = insert_worker(&pool, "DR51", "壬司机", Some(wt), true, false).await;

    let (_, env1) = set_driver(&app, &token, note_id, driver, v0).await;
    let v1 = env1["data"]["version"].as_i64().unwrap() as i32;

    // 窗口期内司机被停用
    sqlx::query("UPDATE t_worker SET is_active = false WHERE id = $1")
        .bind(driver)
        .execute(&pool)
        .await
        .expect("deactivate driver");

    let (s, env) = pickup(&app, &token, note_id, v1).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "指定后被停用的司机不该能领取: {env}"
    );
    assert_eq!(env["code"], 21409);
    let batch_status: String =
        sqlx::query_scalar("SELECT status FROM t_part_batch WHERE delivery_note_id = $1 LIMIT 1")
            .bind(note_id)
            .fetch_one(&pool)
            .await
            .expect("read batch status");
    assert_eq!(batch_status, "READY_TO_SHIP", "领取失败 ⇒ 批次状态不变");
}

/// 同上第二态：指定之后司机被**改工种**（不再是送货司机）⇒ 21409。
#[tokio::test]
async fn pickup_revalidates_driver_whose_work_type_changed() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "领取改工种").await;
    let _l2 = insert_l2(&pool, "领取改工种二厂", l1).await;
    let (note_id, v0) = insert_submitted_note_with_batch(&pool, l1).await;
    let driver_wt = work_type(&pool, "送货司机", "送货司机").await;
    let other_wt = work_type(&pool, "搬运工", "搬运工").await;
    let driver = insert_worker(&pool, "DR61", "癸司机", Some(driver_wt), true, false).await;

    let (_, env1) = set_driver(&app, &token, note_id, driver, v0).await;
    let v1 = env1["data"]["version"].as_i64().unwrap() as i32;

    sqlx::query("UPDATE t_worker SET work_type_id = $1 WHERE id = $2")
        .bind(other_wt)
        .bind(driver)
        .execute(&pool)
        .await
        .expect("change driver work type");

    let (s, env) = pickup(&app, &token, note_id, v1).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21409);
}

/// 单上有非 `READY_TO_SHIP` 批次 ⇒ 21405（领取的批次闸门）。
#[tokio::test]
async fn pickup_with_non_ready_batch_returns_21405() {
    let (pool, app, token) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "领取批次闸门").await;
    let _l2 = insert_l2(&pool, "领取批次闸门二厂", l1).await;
    let (note_id, v0) = insert_submitted_note_with_batch(&pool, l1).await;
    let wt = work_type(&pool, "送货司机", "送货司机").await;
    let driver = insert_worker(&pool, "DR71", "子司机", Some(wt), true, false).await;
    let (_, env1) = set_driver(&app, &token, note_id, driver, v0).await;
    let v1 = env1["data"]["version"].as_i64().unwrap() as i32;

    sqlx::query("UPDATE t_part_batch SET status = 'INSPECTION' WHERE delivery_note_id = $1")
        .bind(note_id)
        .execute(&pool)
        .await
        .expect("downgrade batch status");

    let (s, env) = pickup(&app, &token, note_id, v1).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21405);
}

// ===========================================================================
//  6. 权限
// ===========================================================================

/// Inspector 可指定司机；货架终端 ⇒ 40300。
#[tokio::test]
async fn set_driver_allows_inspector_and_rejects_shelf_account() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let l1 = insert_l1(&pool, "司机权限").await;
    let _l2 = insert_l2(&pool, "司机权限二厂", l1).await;
    let (note_id, v) = insert_submitted_note_with_batch(&pool, l1).await;
    let wt = work_type(&pool, "送货司机", "送货司机").await;
    let driver = insert_worker(&pool, "DR81", "丑司机", Some(wt), true, false).await;
    let app = test_app(test_state(pool.clone()).await);

    let inspector = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    let (s1, env1) = set_driver(&app, &inspector, note_id, driver, v).await;
    assert_eq!(s1, StatusCode::OK, "Inspector 应能指定司机: {env1}");

    let shelf = login_token(&app, &fx.shelf_account_username, PartFixture::PASSWORD).await;
    let (s2, env2) = set_driver(&app, &shelf, note_id, driver, v + 1).await;
    assert_eq!(s2, StatusCode::FORBIDDEN, "货架终端不该能指定司机: {env2}");
    assert_eq!(env2["code"], 40300);
}
