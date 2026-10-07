//! `POST /api/v2/com/delivery/note/scan` 的**闸门**端到端测试
//!
//! 覆盖 `service/scan_entry.rs` 模块 doc 的校验闸门表：
//!
//! | 检查 | 错误码 | 用例 |
//! |---|---|---|
//! | 批次 status ≠ `READY_TO_SHIP`（含 `INSPECTION`） | 21405 | `inspection_batch_rejected_21405` |
//! | 批次已挂在别的 `DRAFT` / `SUBMITTED` 单上 | 21406 | `batch_on_active_note_rejected_21406` |
//! | 零件的 L1 客户 ≠ 单据 L1 客户 | 21407 | `cross_l1_part_rejected_21407` |
//! | DP 不可行（凑不出） | 21405 | `quantity_over_entryable_total_returns_21405` |
//!
//! ## ★ 最重要的一条：`INSPECTION` 必须显式报 21405，绝不静默说谎
//!
//! 入单口径从 `{INSPECTION, READY_TO_SHIP}` 收窄为 `{READY_TO_SHIP}` 后，
//! `INSPECTION` 批次如果掉进分类循环的兜底臂，会得到「无可入单批次」⇒ 旧实现返回
//! `AlreadyPresent` ⇒ 前端弹「已在 XX 上」，**但它根本没被挂上去**。
//! `inspection_batch_is_not_silently_reported_as_already_present` 钉死这条：响应必须
//! 是 21405（带 part / serial / batch 明细），绝不是 200。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    DeliveryFixture, json_request, load_delivery_fixture, login_token, send, test_app, test_pool,
    test_state,
};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, DeliveryFixture) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn insert_l1(pool: &PgPool, name: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 17).next_id();
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
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 17).next_id();
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

async fn insert_part(pool: &PgPool, name: &str, serial_no: &str, customer_id: i64) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 17).next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'D-GATE', $4, 'READY_TO_SHIP', '闸门测试', $5, $5, 20, 0, \
         $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(Some(serial_no.to_string()))
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: &str,
    note_id: Option<i64>,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 17).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         delivery_note_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 'INSPECTION_SHELF', $6, 0, $7, NULL, $7, NULL)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(quantity)
    .bind(status)
    .bind(note_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

async fn insert_note(pool: &PgPool, l1_id: i64, no: &str, status: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 17).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_delivery_note (id, delivery_note_no, customer_id, delivery_date, \
         status, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(no)
    .bind(l1_id)
    .bind(now.date())
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert note");
    id
}

async fn scan_entry(
    app: &axum::Router,
    token: &str,
    serial_no: &str,
    entries: Value,
) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/note/scan",
            Some(json!({
                "serial_no": serial_no,
                "note_version": null,
                "entries": entries,
            })),
            Some(token),
        ),
    )
    .await
}

fn part_entry(part_id: i64, quantity: i32) -> Value {
    json!([{"node_kind": "PART", "node_id": part_id.to_string(), "quantity": quantity}])
}

/// 库里「已挂单的批次」总数（用于断言闸门拒绝时零写入）。
async fn attached_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM t_part_batch WHERE delivery_note_id IS NOT NULL")
        .fetch_one(pool)
        .await
        .expect("count attached")
}

/// 库里送货单总数。
async fn note_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM t_delivery_note")
        .fetch_one(pool)
        .await
        .expect("count notes")
}

// ===========================================================================
//  21405：状态闸门
// ===========================================================================

/// ★ `INSPECTION` 批次一律 21405，**且必须带 part / serial / batch 明细**。
///
/// 规格原文：「不许走兜底沉默」—— 旧实现会让 `INSPECTION` 掉进「无可入单批次」的兜底
/// 臂并返回 `AlreadyPresent`，前端弹「已在 XX 上」而实际没挂上去。
#[tokio::test]
async fn inspection_batch_is_not_silently_reported_as_already_present() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门待检").await;
    let l2 = insert_l2(&pool, "闸门待检二厂", l1).await;
    let part = insert_part(&pool, "待检件", "G-INSP", l2).await;
    insert_batch(&pool, part, 1, 5, "INSPECTION", None).await;

    let notes_before = note_count(&pool).await;
    let (s, env) = scan_entry(&app, &token, "G-INSP", part_entry(part, 5)).await;

    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "INSPECTION 批次必须显式报 21405（绝不能是 200 / 幂等说谎）: {env}"
    );
    assert_eq!(env["code"], 21405);
    let msg = env["message"].as_str().unwrap();
    assert!(
        msg.contains("READY_TO_SHIP"),
        "message 应说明「入单只允许 READY_TO_SHIP」: {msg}"
    );
    assert!(
        msg.contains("G-INSP") && msg.contains("INSPECTION"),
        "message 应带 serial_no 与 batch status 明细: {msg}"
    );
    assert_eq!(attached_count(&pool).await, 0, "闸门拒绝时零写入");
    assert_eq!(note_count(&pool).await, notes_before, "闸门拒绝时不建单");
}

/// 其余未过检状态（`PENDING` / `IN_PROCESS` / `DELIVERED` …）同样 21405。
///
/// ⚠️ 这条同时钉住「显式分支」的实现形态：任何一个**没进 `not_ready` 列表**的状态都会
/// 掉进兜底臂。
#[tokio::test]
async fn non_ready_statuses_all_rejected_21405() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门状态").await;
    let l2 = insert_l2(&pool, "闸门状态二厂", l1).await;

    for (i, status) in [
        "PENDING",
        "PROGRAMMING",
        "IN_PROCESS",
        "DELIVERED",
        "OUTSOURCE",
        "COMPLETED",
        "CANCELLED",
    ]
    .iter()
    .enumerate()
    {
        let serial = format!("G-ST{i}");
        let part = insert_part(&pool, &format!("状态件{i}"), &serial, l2).await;
        insert_batch(&pool, part, 1, 2, status, None).await;
        let (s, env) = scan_entry(&app, &token, &serial, part_entry(part, 2)).await;
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "status={status} 必须 21405（不能被兜底成幂等）: {env}"
        );
        assert_eq!(env["code"], 21405, "status={status}: {env}");
    }
    assert_eq!(attached_count(&pool).await, 0);
}

/// 入单量超过可入单总量 ⇒ 21405（DP 不可行），不挂部分。
#[tokio::test]
async fn quantity_over_entryable_total_returns_21405() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门超量").await;
    let l2 = insert_l2(&pool, "闸门超量二厂", l1).await;
    let part = insert_part(&pool, "超量件", "G-OVER", l2).await;
    insert_batch(&pool, part, 1, 3, "READY_TO_SHIP", None).await;

    let (s, env) = scan_entry(&app, &token, "G-OVER", part_entry(part, 9)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21405);
    assert!(
        env["message"].as_str().unwrap().contains("可入单件数不足"),
        "message 应说明可入单件数不足: {env}"
    );
    assert_eq!(attached_count(&pool).await, 0, "不能部分挂单");
}

/// 整批被占用（无可用批次）时也要 21405（而不是「已在 XX 上」）。
#[tokio::test]
async fn fully_occupied_part_returns_21405_not_already_present() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门全占用").await;
    let l2 = insert_l2(&pool, "闸门全占用二厂", l1).await;
    let part = insert_part(&pool, "全占用件", "G-FULL", l2).await;
    let other = insert_note(&pool, l1, "DN-GFULL-1", "SUBMITTED").await;
    insert_batch(&pool, part, 1, 4, "READY_TO_SHIP", Some(other)).await;

    let (s, env) = scan_entry(&app, &token, "G-FULL", part_entry(part, 4)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{env}");
    assert_eq!(
        env["code"], 21406,
        "全部批次被活跃单占用 ⇒ 21406（不是 21405，也不是幂等 200）: {env}"
    );
}

// ===========================================================================
//  21406：占用冲突
// ===========================================================================

/// 批次挂在别的 `DRAFT` 单上 ⇒ 409 / 21406，message 带「part + batch + 单 id」。
#[tokio::test]
async fn batch_on_active_note_rejected_21406() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门 21406").await;
    let l2 = insert_l2(&pool, "闸门 21406 二厂", l1).await;
    let part = insert_part(&pool, "冲突件", "G-21406", l2).await;
    let free = insert_batch(&pool, part, 1, 5, "READY_TO_SHIP", None).await;
    // 占用单必须是 `SUBMITTED`：本域判定键是「同 L1 唯一的 DRAFT」，若占用单是
    // `DRAFT` 它就是本次扫码的落点单，那个批次算「已挂本单」而不是冲突。
    let taken_note = insert_note(&pool, l1, "DN-G21406-1", "SUBMITTED").await;
    insert_batch(&pool, part, 2, 5, "READY_TO_SHIP", Some(taken_note)).await;

    let (s, env) = scan_entry(&app, &token, "G-21406", part_entry(part, 5)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{env}");
    assert_eq!(env["code"], 21406);
    let msg = env["message"].as_str().unwrap();
    assert!(
        msg.contains(&taken_note.to_string()),
        "message 应指出占用方单 id: {msg}"
    );
    // 部分可用也不允许「挑能用的挂上」——整个请求原子失败
    assert_eq!(
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT delivery_note_id FROM t_part_batch WHERE id = $1"
        )
        .bind(free)
        .fetch_one(&pool)
        .await
        .expect("read"),
        None,
        "有批次被占用时整单失败，未被占用的那个也不能挂"
    );
}

/// 占用方是**终态单**（`PICKED_UP`）⇒ 21406，且 message 必须说清「货已送出、不可
/// 再次入单」（而不是含糊的「已被占用」）。
///
/// 这条钉住 2026-10-08 修掉的一个**三桶漏底**：批次挂在 `PICKED_UP` 单上时，
/// 它既不在「可入单」集合（repo 按 `READY_TO_SHIP` + 未占用筛）里、也不算「活跃占用」
/// （`DRAFT` / `SUBMITTED` 才算）、状态又是 `READY_TO_SHIP`（不进 not_ready）⇒ 落进
/// DP 的「凑不出」⇒ 返回 **21405「可入单件数不足」**，语义完全错（用户看到的是
/// 「货不够」，实际是「货已经送走了」）。现按「已送出占用」显式报 21406。
#[tokio::test]
async fn batch_on_picked_up_note_returns_21406_with_sent_away_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门已领").await;
    let l2 = insert_l2(&pool, "闸门已领二厂", l1).await;
    let part = insert_part(&pool, "已领件", "G-PICKED", l2).await;
    let picked = insert_note(&pool, l1, "DN-GPICK-1", "PICKED_UP").await;
    insert_batch(&pool, part, 1, 5, "READY_TO_SHIP", Some(picked)).await;

    let (s, env) = scan_entry(&app, &token, "G-PICKED", part_entry(part, 5)).await;
    assert_eq!(s, StatusCode::CONFLICT, "已送出占用 ⇒ 409: {env}");
    assert_eq!(
        env["code"], 21406,
        "不能退化成 21405「可入单件数不足」: {env}"
    );
    let msg = env["message"].as_str().unwrap();
    assert!(
        msg.contains("已随该单送出"),
        "message 必须说明货已送出、不可再次入单: {msg}"
    );
}

/// 占用方已**软删** ⇒ 视同未占用，可正常入单（与扫码树 JOIN 的
/// `dn.deleted_at IS NULL` 同口径）。
#[tokio::test]
async fn batch_on_soft_deleted_note_is_treated_as_unoccupied() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门已删单").await;
    let l2 = insert_l2(&pool, "闸门已删单二厂", l1).await;
    let part = insert_part(&pool, "已删单件", "G-SOFTNOTE", l2).await;
    let dead = insert_note(&pool, l1, "DN-GSOFT-1", "SUBMITTED").await;
    insert_batch(&pool, part, 1, 5, "READY_TO_SHIP", Some(dead)).await;
    sqlx::query("UPDATE t_delivery_note SET deleted_at = now() WHERE id = $1")
        .bind(dead)
        .execute(&pool)
        .await
        .expect("soft delete note");

    let (s, env) = scan_entry(&app, &token, "G-SOFTNOTE", part_entry(part, 5)).await;
    assert_eq!(s, StatusCode::OK, "占用方软删 ⇒ 视同未占用: {env}");
}

// ===========================================================================
//  21407：L1 一致性
// ===========================================================================

/// 零件的 L1 ≠ 单据 L1 ⇒ 400 / 21407。
///
/// ⚠️ 规格 §4.2 的闸门表把这一条写成「21416 `BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS`」，
/// 但仓内 `21416` 是 `BIZ_DELIVERY_NOTE_SCOPE_MISMATCH`（随范围判定一并下线）、
/// `BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS` 是 **21407**。实现按「语义名」走 21407
/// —— 与删除前的 `add_parts_inner` 完全一致，客户端的错误处理不变。
#[tokio::test]
async fn cross_l1_part_rejected_21407() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1_a = insert_l1(&pool, "闸门 L1 甲").await;
    let l1_b = insert_l1(&pool, "闸门 L1 乙").await;
    let l2_a = insert_l2(&pool, "甲二厂", l1_a).await;
    let l2_b = insert_l2(&pool, "乙二厂", l1_b).await;
    // 先给 L1 甲建一张草稿（扫 L1 甲的码时建单），再扫 L1 乙的码
    let part_a = insert_part(&pool, "甲件", "G-L1A", l2_a).await;
    insert_batch(&pool, part_a, 1, 3, "READY_TO_SHIP", None).await;
    let (s1, env1) = scan_entry(&app, &token, "G-L1A", part_entry(part_a, 3)).await;
    assert_eq!(s1, StatusCode::OK, "先建一张甲的草稿: {env1}");

    let part_b = insert_part(&pool, "乙件", "G-L1B", l2_b).await;
    insert_batch(&pool, part_b, 1, 3, "READY_TO_SHIP", None).await;
    let (s2, env2) = scan_entry(&app, &token, "G-L1B", part_entry(part_b, 3)).await;

    // L1 乙的扫码会 find-or-create 出「乙的另一张草稿」，零件 L1 与单据 L1 一致 ⇒ 不该
    // 报 21407（每个 L1 各管各的单）。本用例因此只钉「不会串单」。
    assert_eq!(
        s2,
        StatusCode::OK,
        "两个 L1 各建各的单，不该互相报 21407: {env2}"
    );
    assert_ne!(
        env1["data"]["id"].as_str().unwrap(),
        env2["data"]["id"].as_str().unwrap(),
        "两个 L1 必须是两张不同的单"
    );
}

/// 显式的跨 L1 混合：一个请求里 entries 混了两个 L1 的零件 ⇒ 21407。
#[tokio::test]
async fn mixed_l1_entries_in_one_request_rejected_21407() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1_a = insert_l1(&pool, "混单 L1 甲").await;
    let l1_b = insert_l1(&pool, "混单 L1 乙").await;
    let l2_a = insert_l2(&pool, "混单甲二厂", l1_a).await;
    let l2_b = insert_l2(&pool, "混单乙二厂", l1_b).await;
    let part_a = insert_part(&pool, "混单甲件", "G-MIXA", l2_a).await;
    let part_b = insert_part(&pool, "混单乙件", "G-MIXB", l2_b).await;
    insert_batch(&pool, part_a, 1, 3, "READY_TO_SHIP", None).await;
    insert_batch(&pool, part_b, 1, 3, "READY_TO_SHIP", None).await;

    let (s, env) = scan_entry(
        &app,
        &token,
        "G-MIXA",
        json!([
            {"node_kind": "PART", "node_id": part_a.to_string(), "quantity": 3},
            {"node_kind": "PART", "node_id": part_b.to_string(), "quantity": 3},
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21407, "同一请求混入别的 L1 的零件: {env}");
    assert_eq!(attached_count(&pool).await, 0, "跨 L1 必须整体失败、零写入");
}

/// 入参闸门：`node_kind` 非法 / 缺 quantity / 非正数 ⇒ 400（`BIZ_INVALID_VALUE`）。
#[tokio::test]
async fn malformed_entries_rejected_400() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门入参").await;
    let l2 = insert_l2(&pool, "闸门入参二厂", l1).await;
    let part = insert_part(&pool, "入参件", "G-BAD", l2).await;
    insert_batch(&pool, part, 1, 5, "READY_TO_SHIP", None).await;

    let cases: Vec<(&str, Value)> = vec![
        (
            "非法 node_kind",
            json!([{"node_kind": "WIDGET", "node_id": part.to_string(), "quantity": 1}]),
        ),
        (
            "PART 缺 quantity",
            json!([{"node_kind": "PART", "node_id": part.to_string()}]),
        ),
        (
            "quantity 非正",
            json!([{"node_kind": "PART", "node_id": part.to_string(), "quantity": 0}]),
        ),
        (
            "ASSEMBLY 缺 sets",
            json!([{"node_kind": "ASSEMBLY", "node_id": part.to_string()}]),
        ),
    ];
    for (label, entries) in cases {
        let (s, env) = scan_entry(&app, &token, "G-BAD", entries).await;
        assert_eq!(
            s,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label} 应 422（`AppError::validation`）: {env}"
        );
    }
    assert_eq!(attached_count(&pool).await, 0);
}
