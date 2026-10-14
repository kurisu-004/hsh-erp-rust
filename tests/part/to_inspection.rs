//! part 域集成测试 —— to-inspection / batch-to-inspection 流
//!
//! 覆盖：
//!   - to-inspection：happy path ×3（PENDING / PROGRAMMING / IN_PROCESS+PRODUCTION_SHELF）
//!   - to-inspection：IN_PROCESS+WORKER / IN_PROCESS+非 PRODUCTION_SHELF holder 拒绝
//!   - to-inspection：★ `force=true` 强制送检（2026-10-17）—— 工人手上批次放行 /
//!     事件写 `FORCED_SENT_TO_INSPECTION` + `worker_id IS NULL` / OCC 仍生效 /
//!     非生产架与空 location 两支**不放宽** / 部分数量下余量仍留在工人手上 /
//!     **闸门没被绕过时（PENDING + force=true）不写 FORCED_*、note 不带 `[FORCE]`**
//!   - to-inspection：target shelf zone≠INSPECTION / is_active=false 拒绝
//!   - batch-to-inspection：items 空 / 超 200 / 3 件混合 / CLERK 越权
//!   - to-inspection partial-split：PENDING 批次 qty=10 → quantity=3，拆批
//!   - to-inspection：送检出池清 `current_process_id`（2026-09-30 review H2 回归）
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//! 每个用例 INSPECTOR token（在 `batch.transition` 矩阵内）。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::{
    json_request, load_part_fixture, login_token, send, shared_test_snowflake, test_app, test_pool,
    test_state,
};

/// to-inspection 的 `INSPECTED` 事件 note 必须写**货架 code**（人类可读），
/// 不能写 19 位雪花 id。
///
/// 这条 note 直接显示在工单时间线上（`GET /parts/{id}/events`）。自动选架之后
/// `to_inspection_core` 不再自己查架，它拿的是 caller 传下来的 `(id, code)` 配对；
/// 若哪一侧只传了 id（或把 code 换成 id 格式化），时间线就会退化成
/// 「送检：来自待下发 → 品检架 9000000000000000014」这种运维噪声。
#[tokio::test]
async fn to_inspection_event_note_carries_shelf_code_not_id() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (_part_id, batch_id) =
        insert_part_with_batch(&pool, "P0", fx.customer_l2_id, Some("P000"), "PENDING", 5).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({ "version": v })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");

    let note: Option<String> = sqlx::query_scalar(
        "SELECT note FROM t_part_event WHERE batch_id = $1 AND event_type = 'INSPECTED'",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("读 INSPECTED 事件");
    let note = note.expect("INSPECTED 事件应带 note");
    assert!(
        note.contains("FX-SH-INSP"),
        "note 必须含货架 code `FX-SH-INSP`（实际：{note}）"
    );
    assert!(
        !note.contains(&fx.inspection_shelf_id.to_string()),
        "note 不应含货架雪花 id（实际：{note}）"
    );
}

// ===========================================================================
//  动态 fixture helpers（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::insert_shelf` 引入，因 fixtures.rs
//  本轮被删，复制到本地（同形 sqlx::query 直插）。
// ===========================================================================

/// 插一个 t_shelf 行（code / name / zone）。
async fn insert_shelf(pool: &PgPool, code: &str, name: &str, zone: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_test_support::pool_snowflake;

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
//  动态 part/batch 插入 helper（sub-file 私有，PR-C 末统一迁）
// ===========================================================================

async fn insert_part_with_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    status: &str,
    qty: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let part_id = shared_test_snowflake().next_id();
    let batch_id = shared_test_snowflake().next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-001', $4, $7, $3, $5, $5, 1, 0, $6, $6)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, $4, 0, $5, $5)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(qty)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    (part_id, batch_id)
}

async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
}

/// 插一个 `t_worker` 行（`force` 场景需要一个**真实的**持有人，而不是字面量 id）。
///
/// 为什么不复用既有用例里的 `fake_holder: i64 = 999_999_999`：那批用例只需要
/// 「`location='WORKER'`」这一列去撞闸门，holder 是不是真工人无所谓；而本节要断言
/// 「强制送检后 `current_holder_id` 被**覆盖**成品检架 id、原工人不留残留」，
/// 拿一个不存在的 id 当原持有人会把这句断言变成同义反复（覆盖了谁？）。
async fn insert_worker(pool: &PgPool, badge: &str, name: &str, work_type_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, true, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(badge)
    .bind(name)
    .bind(work_type_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

/// 把批次落到指定 `location` / 持有人上（`IN_PROCESS` 组合校验的三种起手形态共用）。
async fn place_batch(pool: &PgPool, batch_id: i64, location: Option<&str>, holder: Option<i64>) {
    sqlx::query("UPDATE t_part_batch SET location = $1, current_holder_id = $2 WHERE id = $3")
        .bind(location)
        .bind(holder)
        .bind(batch_id)
        .execute(pool)
        .await
        .expect("place batch");
}

/// 读批次三元组 `(status, location, current_holder_id)`。
async fn read_placement(pool: &PgPool, batch_id: i64) -> (String, Option<String>, Option<i64>) {
    sqlx::query_as("SELECT status, location, current_holder_id FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("read batch placement")
}

// ===========================================================================
//  bootstrap helpers
// ===========================================================================

async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_clerk() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Tests
// ===========================================================================

/// to-inspection happy path：PENDING → INSPECTION（无 PASS/FAIL 分支）。
#[tokio::test]
async fn to_inspection_from_pending_succeeds() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) =
        insert_part_with_batch(&pool, "P0", fx.customer_l2_id, Some("P000"), "PENDING", 5).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
    assert_eq!(body["data"]["part"]["id"], part_id.to_string());
    assert_eq!(body["data"]["new_batch_id"], serde_json::Value::Null);
}

/// to-inspection happy path：PROGRAMMING → INSPECTION。
#[tokio::test]
async fn to_inspection_from_programming_succeeds() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (_part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PROGRAMMING",
        5,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
}

/// to-inspection happy path：IN_PROCESS + PRODUCTION_SHELF holder → INSPECTION。
///
/// service 层组合校验：IN_PROCESS + 当前 holder 是 PRODUCTION 货架才放行。
#[tokio::test]
async fn to_inspection_from_in_process_production_shelf_succeeds() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (_part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "IN_PROCESS",
        5,
    )
    .await;
    sqlx::query(
        "UPDATE t_part_batch SET location = 'PRODUCTION_SHELF', current_holder_id = $1 \
         WHERE id = $2",
    )
    .bind(fx.production_shelf_id)
    .bind(batch_id)
    .execute(&pool)
    .await
    .unwrap();
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
}

/// to-inspection 回归（2026-09-30 review 第 1 轮 H2）：送检 = **出池**，
/// `current_process_id` 必须被置 NULL。
///
/// 背景：`mark_batch_inspected`（to-inspection 第一步）此前只翻
/// `status` / `location` / `current_holder_id`，不碰 `current_process_id` ——
/// 批次会带着上一道工序的 id 停在 `INSPECTION` 状态，与 migration 004 的列定义
/// 「NULL 表示批次不在生产工序池中」矛盾。
///
/// 不会造成池污染（4 条池 SQL 与 `list_pickable_by_work_type` 都硬限定
/// `IN_PROCESS` + `PRODUCTION_SHELF`），但那条 DB 级不变量必须成立。
#[tokio::test]
async fn to_inspection_clears_current_process_id() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (_part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-CLEAR",
        fx.customer_l2_id,
        Some("P000-CLEAR"),
        "IN_PROCESS",
        5,
    )
    .await;
    // 起手：批次在 P1 工序池里（IN_PROCESS + PRODUCTION_SHELF + current_process_id=P1）
    sqlx::query(
        "UPDATE t_part_batch SET location = 'PRODUCTION_SHELF', current_holder_id = $1, \
         current_process_id = 987654321 \
         WHERE id = $2",
    )
    .bind(fx.production_shelf_id)
    .bind(batch_id)
    .execute(&pool)
    .await
    .unwrap();
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");

    // 出池不变式第 2 行：送检后 current_process_id 必须为 NULL
    let (loc, pid): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT location, current_process_id FROM t_part_batch WHERE id = $1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .expect("query after to-inspection");
    assert_eq!(loc.as_deref(), Some("INSPECTION_SHELF"));
    assert_eq!(
        pid, None,
        "送检 = 出池，current_process_id 必须置 NULL；\
         残留值会让「NULL ⟺ 不在生产工序池中」这条 DB 级不变量失效"
    );
}

/// to-inspection 拒绝：IN_PROCESS + WORKER holder → 20103。
///
/// service 用 `ShelfRepo::get_by_id(current_holder_id)` 返回 None 启发式识别 worker 持有。
#[tokio::test]
async fn to_inspection_in_process_worker_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (_part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "IN_PROCESS",
        5,
    )
    .await;
    let fake_holder: i64 = 999_999_999;
    sqlx::query(
        "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = $1 \
         WHERE id = $2",
    )
    .bind(fake_holder)
    .bind(batch_id)
    .execute(&pool)
    .await
    .unwrap();
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(body["message"].as_str().unwrap().contains("工人持有"));
}

/// to-inspection 拒绝：IN_PROCESS + 非 PRODUCTION_SHELF holder → 20103。
#[tokio::test]
async fn to_inspection_in_process_non_production_shelf_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // 第二个 INSPECTION 货架当 holder（让 part 持有一个非 PRODUCTION 的 shelf）
    let holder_shelf = insert_shelf(&pool, "INSP-002", "品检架B", "INSPECTION").await;
    let (_part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "IN_PROCESS",
        5,
    )
    .await;
    sqlx::query(
        "UPDATE t_part_batch SET location = 'INSPECTION_SHELF', current_holder_id = $1 \
         WHERE id = $2",
    )
    .bind(holder_shelf)
    .bind(batch_id)
    .execute(&pool)
    .await
    .unwrap();
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(body["message"].as_str().unwrap().contains("不在生产架上"));
}

// ===========================================================================
//  强制送检（`force=true`，2026-10-17 新增）
//
//  需求：待品检页扫码后，批次若**压在工人手上**原本无法送检，现在可以直接送检；
//  但事件记为**强制送检**，**不计**该工人的报工统计。
//
//  ⚠️ `force` **只**放宽 `location='WORKER'` 一支；另两支（非生产架 / 空 location）
//     与 OCC 都不动。下面 7 条用例把这条边界逐面钉死（含「闸门没被绕过时事件仍
//     是 INSPECTED」这一面 —— 判据是闸门而不是请求 flag）。
// ===========================================================================

/// ★ 强制送检 happy path：`IN_PROCESS + WORKER + force=true` ⇒ 200，批次落品检架。
///
/// 断言的重点是**持有人被覆盖**：`mark_batch_inspected` 的
/// `new_holder_id = Some(shelf_id)` + `clear_holder_id: false`（= 覆盖写而非清空）
/// ⇒ 原工人 id 必须**不再出现在**批次行上，否则工人额度会被一件已不在手上的货
/// 永久占住（与 `CLAUDE.md`「终态批次一律不在任何人手上」同源的卫生要求）。
#[tokio::test]
async fn to_inspection_force_allows_worker_held_batch() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let worker_id = insert_worker(&pool, "FX-FORCE-1", "强制送检工人", fx.work_type_id).await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-FORCE",
        fx.customer_l2_id,
        Some("P000-FORCE"),
        "IN_PROCESS",
        5,
    )
    .await;
    place_batch(&pool, batch_id, Some("WORKER"), Some(worker_id)).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "version": v,
                "force": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
    assert_eq!(body["data"]["part"]["id"], part_id.to_string());

    let (b_status, location, holder) = read_placement(&pool, batch_id).await;
    assert_eq!(b_status, "INSPECTION", "强制送检后批次状态: {body}");
    assert_eq!(
        location.as_deref(),
        Some("INSPECTION_SHELF"),
        "强制送检后批次必须落到品检架: {body}"
    );
    assert_eq!(
        holder,
        Some(fx.inspection_shelf_id),
        "原工人 id 必须被品检架 id **覆盖**（不是叠加、也不是留残留）"
    );
    assert_ne!(
        holder,
        Some(worker_id),
        "前提钉死：品检架 id 与原工人 id 不同，否则上一条是同义反复"
    );
}

/// ★ 强制送检的事件：`FORCED_SENT_TO_INSPECTION` + `worker_id IS NULL`。
///
/// 这是「不计入该工人报工统计」的**第一道保险**（写侧就不挂工人）：第二道是
/// statistics 侧 `effective_minutes::WHERE_SQL` 的
/// `event_type IN ('RETURNED_TO_SHELF','SENT_TO_INSPECTION')` 白名单天然排除本值
/// （见 `docs/api/statistics.md`）。两道保险都要断言得动 —— 只断言 event_type 不
/// 够：白名单被谁放宽了，本用例查不出来。
#[tokio::test]
async fn to_inspection_force_writes_forced_event_with_null_worker() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let worker_id = insert_worker(&pool, "FX-FORCE-2", "强制送检工人2", fx.work_type_id).await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-FORCE-EV",
        fx.customer_l2_id,
        Some("P000-FORCE-EV"),
        "IN_PROCESS",
        5,
    )
    .await;
    place_batch(&pool, batch_id, Some("WORKER"), Some(worker_id)).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({ "version": v, "force": true })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");

    // 该工单下的事件集合：强制路径**只**写 FORCED_*，绝不能再落一条 INSPECTED
    let mut types: Vec<String> =
        sqlx::query_scalar("SELECT event_type FROM t_part_event WHERE part_id = $1")
            .bind(part_id)
            .fetch_all(&pool)
            .await
            .expect("读工单事件");
    types.sort();
    assert_eq!(
        types,
        vec!["FORCED_SENT_TO_INSPECTION".to_string()],
        "强制送检只应写一条 FORCED_SENT_TO_INSPECTION 事件: {body}"
    );

    let (event_worker, event_process, note): (Option<i64>, Option<i64>, Option<String>) =
        sqlx::query_as(
            "SELECT worker_id, process_id, note FROM t_part_event \
             WHERE part_id = $1 AND event_type = 'FORCED_SENT_TO_INSPECTION'",
        )
        .bind(part_id)
        .fetch_one(&pool)
        .await
        .expect("读强制送检事件行");
    assert_eq!(
        event_worker, None,
        "强制送检的 worker_id 必须为 NULL（**绝不**填原工人 id）—— 否则会算进他的报工统计"
    );
    assert_eq!(event_process, None, "process_id 同理保持 NULL");
    let note = note.expect("强制送检事件应带 note");
    assert!(
        note.starts_with("[FORCE] "),
        "note 必须带 [FORCE] 前缀（对齐 FORCE_COMPLETED 的口径）: {note}"
    );
    assert!(
        note.contains("FX-SH-INSP"),
        "自动生成的 note 仍须含货架 code: {note}"
    );
    // 2026-10-17 review 第 2 轮 NIT-D：强制路径的自动 note **单独措辞**。
    // `from` 只含状态不含位置，而这批货的真实位置是「工人手上」—— 沿用普通路径那句
    // 「来自生产架」会在时间线上留下一条自相矛盾的人读文本（生产架上没有这批货）。
    assert!(
        note.contains("强制取自工人"),
        "强制送检的自动 note 必须说清来源是工人: {note}"
    );
    assert!(
        !note.contains("来自生产架"),
        "强制送检时这批货不在生产架上，note 不得写「来自生产架」: {note}"
    );
}

/// `force` **不绕过** OCC：version 陈旧仍 40901。
///
/// 防止「加了个跳过所有校验的 force 分支」这种实现 —— 运营点按钮时页面上的快照同样
/// 可能过期，放宽 OCC 会让并发下的最后一次覆盖静默成功。
#[tokio::test]
async fn to_inspection_force_does_not_bypass_occ() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let worker_id = insert_worker(&pool, "FX-FORCE-3", "强制送检工人3", fx.work_type_id).await;
    let (_part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-FORCE-OCC",
        fx.customer_l2_id,
        Some("P000-FORCE-OCC"),
        "IN_PROCESS",
        5,
    )
    .await;
    place_batch(&pool, batch_id, Some("WORKER"), Some(worker_id)).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({ "version": v + 999, "force": true })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body={body}");
    assert_eq!(body["code"], 40901, "force 不得绕过 OCC: {body}");

    // 拒绝后批次原地不动（闸门在写之前）
    let (b_status, location, holder) = read_placement(&pool, batch_id).await;
    assert_eq!(b_status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(worker_id));
}

/// `force=true` **不放宽**空 `location` 那一支（数据完整性违规 ⇒ 仍 20103）。
#[tokio::test]
async fn to_inspection_force_still_rejects_null_location() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (_part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-FORCE-NULL",
        fx.customer_l2_id,
        Some("P000-FORCE-NULL"),
        "IN_PROCESS",
        5,
    )
    .await;
    place_batch(&pool, batch_id, None, None).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({ "version": v, "force": true })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("batch.location 为空"),
        "force 只放宽 WORKER 一支，空 location 的报错文案不应变: {body}"
    );
}

/// `force=true` **不放宽**非生产架（`OFFICE`）那一支 ⇒ 仍 20103。
///
/// `OFFICE` 是「这批货压根不在生产流里」（已交付待发货区），与「在工人手上」语义
/// 完全不同；需求没要求放宽它。
#[tokio::test]
async fn to_inspection_force_still_rejects_non_production_shelf_location() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (_part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-FORCE-OFF",
        fx.customer_l2_id,
        Some("P000-FORCE-OFF"),
        "IN_PROCESS",
        5,
    )
    .await;
    place_batch(&pool, batch_id, Some("OFFICE"), None).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({ "version": v, "force": true })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20103);
    assert!(
        body["message"].as_str().unwrap().contains("不在生产架上"),
        "force 只放宽 WORKER 一支，非生产架的报错文案不应变: {body}"
    );
}

/// ★ 强制送检 + 部分数量：operated 批次进品检架，**余量批次仍留在工人手上**。
///
/// 拆批走 `_split_batch_inner` 的 `INSERT ... SELECT`：新拆出的 operated 批次
/// **继承**源批次的 `location='WORKER'` + 原工人 id，随后被 `mark_batch_inspected`
/// 翻成 `INSPECTION_SHELF` + 品检架；而**余量批次（源批次本身）原地不动**。
/// 这是**语义正确**的（工人确实还拿着那部分），因此刻意不清 —— 本用例把它钉死，
/// 免得后人当「残留」去"修"。
#[tokio::test]
async fn to_inspection_force_partial_quantity_leaves_remainder_on_worker() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let worker_id = insert_worker(&pool, "FX-FORCE-4", "强制送检工人4", fx.work_type_id).await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-FORCE-QTY",
        fx.customer_l2_id,
        Some("P000-FORCE-QTY"),
        "IN_PROCESS",
        5,
    )
    .await;
    place_batch(&pool, batch_id, Some("WORKER"), Some(worker_id)).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({ "version": v, "force": true, "quantity": 3 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(
        body["data"]["new_batch_id"],
        batch_id.to_string(),
        "remainder id 应回填为源批次 id（余量留在源批次）: {body}"
    );

    // operated（新拆出的）批次：已进品检架
    let operated_id: i64 = sqlx::query_scalar(
        "SELECT id FROM t_part_batch WHERE part_id = $1 AND status = 'INSPECTION' \
         AND deleted_at IS NULL",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .expect("读 operated 批次");
    let (op_status, op_loc, op_holder) = read_placement(&pool, operated_id).await;
    assert_eq!(op_status, "INSPECTION");
    assert_eq!(op_loc.as_deref(), Some("INSPECTION_SHELF"));
    assert_eq!(op_holder, Some(fx.inspection_shelf_id));

    // 余量批次（源批次）：仍在工人手上
    let (rem_status, rem_loc, rem_holder): (String, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT status, location, current_holder_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("读余量批次");
    assert_eq!(rem_status, "IN_PROCESS", "余量批次状态不变: {body}");
    assert_eq!(
        rem_loc.as_deref(),
        Some("WORKER"),
        "余量仍在工人手上（语义正确，刻意不清）: {body}"
    );
    assert_eq!(
        rem_holder,
        Some(worker_id),
        "余量的持有人仍是原工人: {body}"
    );
    let rem_qty: i32 = sqlx::query_scalar("SELECT quantity FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("读余量数量");
    assert_eq!(rem_qty, 2, "余量 = 5 - 3");
}

/// `force` 的 `[FORCE]` 前缀加在**最终 note** 上，caller 自带 note 时也照加。
///
/// 若只在自动生成的 note 上加前缀，调用方一旦传了 note（品检页的备注框）强制痕迹
/// 就整个消失，时间线分不出这次是不是强制送检。
#[tokio::test]
async fn to_inspection_force_prefixes_caller_supplied_note() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let worker_id = insert_worker(&pool, "FX-FORCE-5", "强制送检工人5", fx.work_type_id).await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-FORCE-N",
        fx.customer_l2_id,
        Some("P000-FORCE-N"),
        "IN_PROCESS",
        5,
    )
    .await;
    place_batch(&pool, batch_id, Some("WORKER"), Some(worker_id)).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({ "version": v, "force": true, "note": "工人离场，运营代送检" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");

    let note: String = sqlx::query_scalar(
        "SELECT note FROM t_part_event WHERE part_id = $1 AND event_type = 'FORCED_SENT_TO_INSPECTION'",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .expect("读强制送检事件 note");
    assert_eq!(
        note, "[FORCE] 工人离场，运营代送检",
        "caller 自带 note 时也必须保留 [FORCE] 前缀: {note}"
    );
}

/// ★ `force=true` 打**非工人持有**批次 ⇒ 事件仍是 `INSPECTED`、note **不带**
/// `[FORCE]`（2026-10-17 review 第 1 轮 MINOR-4 收紧）。
///
/// 组合校验（工人持有 / 非生产架 / 空 location 那三支）**只在 `from == IN_PROCESS`
/// 时执行** ⇒ PENDING / PROGRAMMING 批次带 `force=true` 时**根本没绕过任何闸门**。
/// 若事件类型与 note 前缀按请求 flag 判定，前端一旦图省事「总是带 force」，
/// `FORCED_SENT_TO_INSPECTION` 与 `[FORCE]` 就会在整条时间线上退化成噪声，
/// 与「运营代工人做的一次越权送检」这层语义（statistics 侧据此不计该工人工时、
/// 前端据此渲染特殊文案）直接冲突。
///
/// 因此本用例与 `to_inspection_force_writes_forced_event_with_null_worker` 成对：
/// 那条钉 `IN_PROCESS + WORKER + force=true` ⇒ `FORCED_*` + `[FORCE]`；本条钉
/// `PENDING + force=true` ⇒ 普通 `INSPECTED` + 无前缀。
#[tokio::test]
async fn to_inspection_force_on_pending_batch_is_not_a_forced_event() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0-F-PEND",
        fx.customer_l2_id,
        Some("P000-F-PEND"),
        "PENDING",
        5,
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({ "version": v, "force": true })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "PENDING 送检本来就放行: {body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION", "{body}");

    let mut types: Vec<String> =
        sqlx::query_scalar("SELECT event_type FROM t_part_event WHERE part_id = $1")
            .bind(part_id)
            .fetch_all(&pool)
            .await
            .expect("读工单事件");
    types.sort();
    assert_eq!(
        types,
        vec!["INSPECTED".to_string()],
        "PENDING 批次带 force=true 时**没绕过任何闸门** ⇒ 不该写 FORCED_*: {body}"
    );

    let note: Option<String> =
        sqlx::query_scalar("SELECT note FROM t_part_event WHERE part_id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("读送检事件 note");
    let note = note.expect("送检事件应带 note");
    assert!(
        !note.starts_with("[FORCE]"),
        "PENDING + force=true 没有闸门被绕过 ⇒ note 不该带 [FORCE] 前缀: {note}"
    );
    assert!(
        note.contains("FX-SH-INSP"),
        "普通送检的自动 note 仍须含货架 code: {note}"
    );
}

/// 2026-10-10：`target_inspection_shelf_id` 已删除 ⇒ 「指定一个 PRODUCTION 架当
/// 品检架」这条错误路径**不再存在**。
///
/// 原用例断言 20511（zone ≠ INSPECTION）。现在选架的候选集硬限定
/// `zone = 'INSPECTION'`，于是「请求体里带一个生产架 id」这件事被 serde 静默忽略
/// —— 请求会**成功**，批次落在自动选出的品检架上。
///
/// 本用例断言的正是这个新语义（老客户端多发 `target_inspection_shelf_id` 不破）。
#[tokio::test]
async fn to_inspection_ignores_removed_target_shelf_field() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (_part_id, batch_id) =
        insert_part_with_batch(&pool, "P0", fx.customer_l2_id, Some("P000"), "PENDING", 5).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                // 已移除的字段：老客户端仍在发（且故意给一个 PRODUCTION 架），必须被静默忽略
                "target_inspection_shelf_id": fx.production_shelf_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let holder: i64 =
        sqlx::query_scalar("SELECT current_holder_id FROM t_part_batch WHERE id = $1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .expect("read back holder");
    assert_eq!(
        holder, fx.inspection_shelf_id,
        "批次必须落在服务端选出的品检架上，而不是请求里那个生产架"
    );
}

/// 2026-10-10：停用的品检架不再被选（原先是 20512 `BIZ_SHELF_INACTIVE`）。
///
/// 错误码随之变成 `40301 SHELF_MISMATCH`：唯一可用品检架被停用后候选为空，而
/// 「选不出品检架」在写侧的统一语义是「你无权访问任何可用的 INSPECTION 货架」
/// （见 `shared::shelf::select::no_candidate_in_scope` 与 `docs/api/batch.md`）。
#[tokio::test]
async fn to_inspection_all_shelves_inactive_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // 把品检架置为 inactive —— 候选集里一个都不剩
    sqlx::query!(
        "UPDATE t_shelf SET is_active = false WHERE id = $1",
        fx.inspection_shelf_id
    )
    .execute(&pool)
    .await
    .unwrap();
    let (_part_id, batch_id) =
        insert_part_with_batch(&pool, "P0", fx.customer_l2_id, Some("P000"), "PENDING", 5).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body={body}");
    assert_eq!(body["code"], 40301);
}

/// batch-to-inspection 拒绝：items 为空 → 422 / 40001 VALIDATION_ERROR。
#[tokio::test]
async fn batch_to_inspection_empty_items_rejected() {
    let (_pool, app, token, fx) = bootstrap_as_inspector().await;
    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/to-inspection",
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "items": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(body["code"], 40001);
}

/// batch-to-inspection 拒绝：items 数量 > 200 → 422 / 40001 VALIDATION_ERROR。
#[tokio::test]
async fn batch_to_inspection_too_many_items_rejected() {
    let (_pool, app, token, fx) = bootstrap_as_inspector().await;
    let items: Vec<i64> = (1..=201).collect();
    let item_payloads: Vec<Value> = items
        .iter()
        .map(|id| json!({ "batch_id": id.to_string(), "version": 0 }))
        .collect();

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/to-inspection",
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "items": item_payloads,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(body["code"], 40001);
}

/// batch-to-inspection 部分成功：3 件混合 → 2 submitted + 1 failed (20103)。
#[tokio::test]
async fn batch_to_inspection_mixed_partial_success() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;

    // 第 1 件：PENDING → 应成功（status=INSPECTION）
    let (_p1, b1) =
        insert_part_with_batch(&pool, "P1", fx.customer_l2_id, Some("P001"), "PENDING", 5).await;
    // 第 2 件：PROGRAMMING → 应成功
    let (_p2, b2) = insert_part_with_batch(
        &pool,
        "P2",
        fx.customer_l2_id,
        Some("P002"),
        "PROGRAMMING",
        5,
    )
    .await;
    // 第 3 件：IN_PROCESS + fake holder → 应失败 (20103)
    let (_p3, b3) = insert_part_with_batch(
        &pool,
        "P3",
        fx.customer_l2_id,
        Some("P003"),
        "IN_PROCESS",
        5,
    )
    .await;
    sqlx::query!(
        "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = 999999999 \
         WHERE id = $1",
        b3
    )
    .execute(&pool)
    .await
    .unwrap();
    let (v1, v2, v3) = (
        batch_version(&pool, b1).await,
        batch_version(&pool, b2).await,
        batch_version(&pool, b3).await,
    );

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/to-inspection",
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "items": [
                    { "batch_id": b1.to_string(), "version": v1 },
                    { "batch_id": b2.to_string(), "version": v2 },
                    { "batch_id": b3.to_string(), "version": v3 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["submitted"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"]["failed"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["failed"][0]["code"], 20103);
    assert_eq!(body["data"]["failed"][0]["batch_id"], b3.to_string());
}

/// batch-to-inspection 权限：CLERK 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn batch_to_inspection_clerk_forbidden() {
    let (_pool, app, token, fx) = bootstrap_as_clerk().await;
    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/to-inspection",
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "items": [{ "batch_id": "1", "version": 0 }],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body={body}");
    assert_eq!(body["code"], 40300);
}

/// to-inspection partial-split happy path：PENDING 批次 qty=10 → quantity=3 → 拆批。
#[tokio::test]
async fn to_inspection_partial_split_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) =
        insert_part_with_batch(&pool, "P0", fx.customer_l2_id, Some("P000"), "PENDING", 10).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "version": v,
                "quantity": 3,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    // PR-B2 §5 改造：part 派生列由 batch rollup 决定。
    assert_eq!(body["data"]["part"]["status"], "PENDING");
    // operated 子批（qty=3）应已翻转到 INSPECTION（DB 直查）
    let operated_status: String = sqlx::query_scalar(
        "SELECT status FROM t_part_batch WHERE part_id = $1 AND status = 'INSPECTION' AND deleted_at IS NULL",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .expect("read operated batch status");
    assert_eq!(
        operated_status, "INSPECTION",
        "operated 子批应已翻转到 INSPECTION"
    );
    let new_bid_str = body["data"]["new_batch_id"]
        .as_str()
        .expect("new_batch_id 应为 string (Some)");
    assert_eq!(
        new_bid_str,
        batch_id.to_string(),
        "remainder id 应回填为源批次 id"
    );
}

// ===========================================================================
//  Scan context 端点（`GET /parts/by-serial/{serial_no}/part-batches`）
// ===========================================================================

/// Scan context happy path：PENDING → to-inspection → by-serial/part-batches → to-ship
/// 端到端验证。
#[tokio::test]
async fn part_batches_returns_narrow_part_and_batches_with_holder() {
    let (pool, app, insp_token, fx) = bootstrap_as_inspector().await;

    // 唯一 serial 避免并发 / 历史数据碰撞
    let serial_no = format!(
        "T-S-{:08}",
        chrono::Utc::now().timestamp_millis() % 100_000_000
    );

    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P-SCAN",
        fx.customer_l2_id,
        Some(&serial_no),
        "PENDING",
        5,
    )
    .await;
    let v_initial = batch_version(&pool, batch_id).await;

    // Step 1：INSPECTOR 触发 to-inspection
    let (status, body) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": fx.inspection_shelf_id.to_string(),
                "version": v_initial,
            })),
            Some(&insp_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "to-inspection: body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");

    // Step 2：复用同一 pool + fixture + app，直接换 MANAGER token
    // （再调 bootstrap_as_manager 会建新 DB，前面的 part 丢失）
    let mgr_token = login_token(&app, PartFixture::MANAGER_USERNAME, PartFixture::PASSWORD).await;
    let (status2, body2) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/parts/by-serial/{serial_no}/part-batches"),
            None::<Value>,
            Some(&mgr_token),
        ),
    )
    .await;
    assert_eq!(status2, StatusCode::OK, "scan-context: body={body2}");
    assert_eq!(body2["code"], 0);

    // ② 工单窄字段断言
    let part = &body2["data"]["part"];
    assert_eq!(part["id"], part_id.to_string(), "part.id 应等于 part_id");
    assert_eq!(part["drawing_no"], "D-001", "part.drawing_no 应为 'D-001'");
    assert!(
        part["customer_id"].is_string(),
        "customer_id 应序列化为 string: {part}"
    );
    assert_eq!(
        part["customer_id"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        fx.customer_l2_id,
        "customer_id 应等于 l2 id"
    );
    // PartScanInfoOut 不含 serial_no / status / version 等多余字段
    assert!(
        part.get("serial_no").is_none(),
        "scan-info 不应含 serial_no: {part}"
    );
    assert!(
        part.get("status").is_none(),
        "scan-info 不应含 status: {part}"
    );
    assert!(
        part.get("version").is_none(),
        "scan-info 不应含 version: {part}"
    );

    // ③ 批次数组断言：1 条 INSPECTION 批次 + 解析后的 holder_name
    let batches = body2["data"]["batches"]
        .as_array()
        .expect("batches 应为 array");
    assert_eq!(batches.len(), 1, "应仅有 1 条活跃批次: {batches:?}");
    assert_eq!(batches[0]["status"], "INSPECTION");
    assert_eq!(batches[0]["quantity"], 5);
    assert!(
        batches[0]["holder_name"].is_string(),
        "INSPECTION 阶段 holder_name 应为 Some: {batches:?}"
    );
    assert_eq!(
        batches[0]["holder_name"].as_str().unwrap(),
        "FX 检验架",
        "holder_name 应解析为品检架名称"
    );
    assert!(
        batches[0]["version"].as_i64().unwrap() > 0,
        "to-inspection 后 batch.version 应递增: {batches:?}"
    );
    let scan_batch_id_str = batches[0]["id"].as_str().expect("batches[0].id is string");
    let scan_batch_version = batches[0]["version"].as_i64().unwrap() as i32;

    // ④ 链式调 to-ship：扫码弹窗 → 品检通过（核心验收）
    let (status3, body3) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{scan_batch_id_str}/to-ship"),
            Some(json!({
                "version": scan_batch_version,
                "quantity": serde_json::Value::Null,
            })),
            Some(&mgr_token),
        ),
    )
    .await;
    assert_eq!(
        status3,
        StatusCode::OK,
        "扫码弹窗 → to-ship 端到端应 200: body={body3}"
    );
    assert_eq!(body3["code"], 0);
    assert_eq!(
        body3["data"]["part"]["status"], "READY_TO_SHIP",
        "to-ship 后 part.status 应翻转为 READY_TO_SHIP: {body3}"
    );
}

/// Scan context not-found：serial 不存在 → 20101 BIZ_PART_NOT_FOUND。
#[tokio::test]
async fn part_batches_serial_not_found_returns_20101() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (status, body) = send(
        app,
        json_request(
            "GET",
            "/parts/by-serial/NOT-EXIST-SCAN-CONTEXT/part-batches",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "serial 不存在应映射 404: body={body}"
    );
    assert_eq!(body["code"], 20101, "BIZ_PART_NOT_FOUND: body={body}");
    let msg = body["message"].as_str().expect("message 应为 string");
    assert!(
        msg.contains("不存在"),
        "message 应含 '不存在'（与 Python 契约对齐）: {msg}"
    );
}

/// Scan context 角色守卫：ShelfAccount 越权 → 403 / 40300 FORBIDDEN。
#[tokio::test]
async fn part_batches_role_guard_rejects_unauthorized() {
    let (pool, _app, _token, fx) = bootstrap_as_manager().await;
    let serial_no = format!(
        "T-RG-{:08}",
        chrono::Utc::now().timestamp_millis() % 100_000_000
    );
    let (_pid, _bid) = insert_part_with_batch(
        &pool,
        "P-RG",
        fx.customer_l2_id,
        Some(&serial_no),
        "PENDING",
        1,
    )
    .await;

    // 用 fixture 的 SHELF_ACCOUNT 用户登录（合法登录但越权）
    let shelf_app = test_app(test_state(pool.clone()).await);
    let shelf_token = login_token(
        &shelf_app,
        PartFixture::SHELF_ACCOUNT_USERNAME,
        PartFixture::PASSWORD,
    )
    .await;
    let (status, body) = send(
        shelf_app,
        json_request(
            "GET",
            &format!("/parts/by-serial/{serial_no}/part-batches"),
            None::<Value>,
            Some(&shelf_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ShelfAccount 越权应 403: body={body}"
    );
    assert_eq!(body["code"], 40300, "FORBIDDEN: body={body}");
}
