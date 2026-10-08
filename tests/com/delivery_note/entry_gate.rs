//! `POST /api/v2/com/delivery/note/scan` 的**闸门**端到端测试
//!
//! 覆盖 `service/scan_entry.rs` 模块 doc 的校验闸门表：
//!
//! | 检查 | 错误码 | 用例 |
//! |---|---|---|
//! | 批次 status ≠ `READY_TO_SHIP`（含 `INSPECTION`）**且该零件可入单量凑不出本次要的量** | 21405 | `inspection_batch_is_not_silently_reported_as_already_present`、`non_ready_statuses_all_rejected_21405`、`mixed_status_part_shortage_lists_blocked_batches` |
//! | 同零件另有足量 `READY_TO_SHIP` 批次 ⇒ 非 READY 批次**不**顺带拒绝 | 200 | `mixed_status_part_enters_from_ready_batch_only` |
//! | 批次已挂在别的 `DRAFT` / `SUBMITTED` 单上（**请求级**闸门） | 21406 | `batch_on_active_note_rejected_21406` |
//! | 零件的 L1 客户 ≠ 单据 L1 客户 | 21407 | `cross_l1_part_rejected_21407` |
//! | DP 不可行（凑不出） | 21405 | `quantity_over_entryable_total_returns_21405` |
//! | 跨 part 原子性：一个 part 够、另一个凑不出 ⇒ **整个请求零写入** | 21405 | `multi_part_shortage_writes_nothing_across_parts` |
//! | ≥2 个 part 全部凑不出 ⇒ message 是**多段**、按 `part_id` 升序、每段带 part 身份 | 21405 | `multi_part_failures_are_listed_in_part_id_order` |
//!
//! ## ★ 最重要的一条：`INSPECTION` 必须显式报 21405，且 message 点名该批次
//!
//! `INSPECTION` 批次不在可入单集合里（可入单只放 `READY_TO_SHIP` 且未占用的活跃批次）。
//! 若分类循环不显式收集它，这些批次在整条链路上没有任何一处会提到：DP 只能报「可入单件数
//! 不足」，用户看到的是「货不够」，真实原因却是「还没品检完」⇒ 状态信息丢失。
//! `inspection_batch_is_not_silently_reported_as_already_present` 钉死这条：响应必须是
//! 21405，且 message 带 serial_no / batch_no / status 明细，绝不是 200。
//!
//! ## 状态闸门的作用域（2026-10-09 修）
//!
//! 同零件存在非 `READY_TO_SHIP` 批次**不再顺带拒绝**整个请求 —— 判定权在 DP 能否凑出
//! 本次要的量：`mixed_status_part_enters_from_ready_batch_only` 钉住「够就成功」，
//! `mixed_status_part_shortage_lists_blocked_batches` 钉住「不够才 21405，且 message
//! 要点名被拦下的批次」。
//!
//! ## 跨 part 原子性与失败汇总的形状
//!
//! DP 循环收集完全部失败 part 才决定拒绝 ⇒ 同一个请求里「A part 分配成功 + B part 凑不出」
//! 时，A 的批次也**不挂单**。`multi_part_shortage_writes_nothing_across_parts` 钉死这条
//! 不变量（零写入）。
//!
//! message 的形状分两片由 `multi_part_failures_are_listed_in_part_id_order` 钉：请求含
//! ≥2 个 part 时每段带 `part {id}（需 N 件）：`，多个失败 part **按 `part_id` 升序**拼接
//! （`targets` 是 `HashMap`、迭代序不定，不排序会让同一请求的 message 抖动）。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    DeliveryFixture, json_request, load_delivery_fixture, login_token, pool_snowflake, send,
    test_app, test_pool, test_state,
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

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, DeliveryFixture) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token, fx)
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

async fn insert_part(pool: &PgPool, name: &str, serial_no: &str, customer_id: i64) -> i64 {
    let id = next_id();
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
    let id = next_id();
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
    let id = next_id();
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

/// 某个批次当前挂在哪张单上（`None` = 未占用）。
async fn batch_note_id(pool: &PgPool, batch_id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("read batch note id")
}

// ===========================================================================
//  21405：状态闸门
// ===========================================================================

/// ★ `INSPECTION` 批次一律 21405，**且必须带 part / serial / batch 明细**。
///
/// 不显式收集 `INSPECTION` 的代价不是「报错了码」，而是**状态信息彻底丢失**：它不在可入单
/// 集合里，DP 只会报一句无主语的「可入单件数不足」，用户无从知道要先去品检。
///
/// ⚠️ 函数名里的 `already_present` 沿用的是早期实现（有「幂等 200」路径）的命名；当前
/// 实现里 200 只可能是「入单成功」，本用例断言的是上面那条 21405 + 明细。
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
        "INSPECTION 批次必须显式报 21405（绝不能是 200）: {env}"
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
/// ⚠️ 这条同时钉住「显式分支」的实现形态：任何一个**没进 `not_ready_by_part`** 的状态都会
/// 在 message 里丢掉自己的名字（只剩「可入单件数不足」）。
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
            "status={status} 必须 21405（message 要点名该状态）: {env}"
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

/// 同零件既有 `IN_PROCESS` 又有足量 `READY_TO_SHIP` 批次 ⇒ **入单成功**，非 READY
/// 批次只「不参与分配」，不构成顺带拒绝的理由。
///
/// 2026-10-09 修的原缺陷：状态闸门作用域被放大成「该零件的全部活跃批次」，本场景
/// 整单 21405，而实际有 8 件 `READY_TO_SHIP` 完全能入单。
#[tokio::test]
async fn mixed_status_part_enters_from_ready_batch_only() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门混状态").await;
    let l2 = insert_l2(&pool, "闸门混状态二厂", l1).await;
    let part = insert_part(&pool, "混状态件", "G-MIXOK", l2).await;
    let in_process = insert_batch(&pool, part, 1, 8, "IN_PROCESS", None).await;
    let ready = insert_batch(&pool, part, 2, 8, "READY_TO_SHIP", None).await;

    let (s, env) = scan_entry(&app, &token, "G-MIXOK", part_entry(part, 8)).await;

    assert_eq!(
        s,
        StatusCode::OK,
        "有足量 READY_TO_SHIP 批次时不该被 IN_PROCESS 批次顺带拒绝: {env}"
    );
    let items = env["data"]["line_items"].as_array().expect("line_items");
    assert_eq!(items.len(), 1, "只应挂上批次2 一行: {env}");
    assert_eq!(
        items[0]["id"].as_str().unwrap(),
        ready.to_string(),
        "行项只能是 READY_TO_SHIP 的批次2: {env}"
    );
    assert_eq!(attached_count(&pool).await, 1, "只挂一个批次: {env}");
    assert!(
        batch_note_id(&pool, ready).await.is_some(),
        "批次2 应被挂上单"
    );
    assert_eq!(
        batch_note_id(&pool, in_process).await,
        None,
        "批次1（IN_PROCESS）必须保持未挂单"
    );
}

/// 同零件有 `IN_PROCESS` 批次、可入单量却凑不出本次要的量 ⇒ 21405，且 message 要
/// 点名被拦下的批次（状态 + serial_no），不部分挂单。
#[tokio::test]
async fn mixed_status_part_shortage_lists_blocked_batches() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门混状态缺货").await;
    let l2 = insert_l2(&pool, "闸门混状态缺货二厂", l1).await;
    let part = insert_part(&pool, "混状态缺货件", "G-MIXSHORT", l2).await;
    insert_batch(&pool, part, 1, 8, "IN_PROCESS", None).await;
    insert_batch(&pool, part, 2, 4, "READY_TO_SHIP", None).await;

    let (s, env) = scan_entry(&app, &token, "G-MIXSHORT", part_entry(part, 8)).await;

    assert_eq!(s, StatusCode::BAD_REQUEST, "可入单 4 件凑不出 8 件: {env}");
    assert_eq!(env["code"], 21405, "{env}");
    let msg = env["message"].as_str().unwrap();
    assert!(
        msg.contains("可入单件数不足"),
        "message 基底应是 DP 的「可入单件数不足」: {msg}"
    );
    assert!(
        msg.contains("G-MIXSHORT") && msg.contains("IN_PROCESS"),
        "message 应点名被状态闸门拦下的批次（serial_no + status）: {msg}"
    );
    assert_eq!(
        attached_count(&pool).await,
        0,
        "凑不出时整请求原子失败，不部分挂单: {env}"
    );
}

/// ★ 跨 part 原子性：一个 part 够、另一个凑不出 ⇒ **整请求零写入**。
///
/// DP 循环收集完全部失败 part 才决定拒绝。本用例钉的是**可观测**的结果：A 的 DP 成功
/// （A 有 8 件足量 `READY_TO_SHIP`、本次正要 8 件，而失败段是穷举的 —— `!msg.contains(A)`
/// 即 A 未失败）却零写入 ⇒ 跨 part 原子失败。
///
/// ⚠️ 别指望它能钉「拒绝点在第一个写之前」那条结构性属性：拆批 / 挂单与草稿行的
/// find-or-create 全在 handler 开的那**同一条事务**里，handler 只在 `Ok` 时 commit ⇒
/// 把写操作挪进 DP 循环（每个 part 分配成功就立刻挂），返回 `Err` 后整体回滚，库里
/// 的结果与现在完全相同（雪花号消耗、行写入都回滚，DB 上不可观测）。任何集成测试都
/// 区分不了这两种写法，要区分只能靠 `sqlx::query!` 的编译期断言或事务内快照。
#[tokio::test]
async fn multi_part_shortage_writes_nothing_across_parts() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门跨 part").await;
    let l2 = insert_l2(&pool, "闸门跨 part 二厂", l1).await;
    // A：足量的 READY_TO_SHIP 批次（8 件，本次要 8 件 ⇒ DP 必然成功）
    let part_a = insert_part(&pool, "跨 part 甲件", "G-XPARTA", l2).await;
    let ready_a = insert_batch(&pool, part_a, 1, 8, "READY_TO_SHIP", None).await;
    // B：只有 4 件可入单，另 8 件卡在 IN_PROCESS ⇒ 本次要 8 件 ⇒ 必然凑不出
    let part_b = insert_part(&pool, "跨 part 乙件", "G-XPARTB", l2).await;
    insert_batch(&pool, part_b, 1, 8, "IN_PROCESS", None).await;
    insert_batch(&pool, part_b, 2, 4, "READY_TO_SHIP", None).await;

    let (s, env) = scan_entry(
        &app,
        &token,
        "G-XPARTA",
        json!([
            {"node_kind": "PART", "node_id": part_a.to_string(), "quantity": 8},
            {"node_kind": "PART", "node_id": part_b.to_string(), "quantity": 8},
        ]),
    )
    .await;

    assert_eq!(s, StatusCode::BAD_REQUEST, "B 凑不出 ⇒ 整请求 21405: {env}");
    assert_eq!(env["code"], 21405, "{env}");
    let msg = env["message"].as_str().unwrap();
    assert!(
        msg.contains("可入单件数不足"),
        "message 基底应是 DP 的「可入单件数不足」: {msg}"
    );
    assert!(
        msg.contains(&part_b.to_string()),
        "请求含多个 part 时失败段必须带 part 身份（B 的 part_id）: {msg}"
    );
    assert!(
        msg.contains("G-XPARTB") && msg.contains("IN_PROCESS"),
        "message 应点名被拦下的批次（serial_no + status）: {msg}"
    );
    assert!(
        !msg.contains("G-XPARTA"),
        "分配成功的 A 不该出现在失败汇总里: {msg}"
    );
    // message 里的失败段是**穷举**的（DP 循环把全部失败 part 一次收齐再汇总），所以 A 的
    // part_id 不出现即证明 A 的 DP 成功 —— 上面「零写入」才是「够量的那个也没被挂」。
    assert!(
        !msg.contains(&part_a.to_string()),
        "A 的 DP 成功 ⇒ 它的 part_id 不该进失败汇总: {msg}"
    );
    assert_eq!(
        attached_count(&pool).await,
        0,
        "跨 part 原子失败：一个 part 凑不出 ⇒ 够量的那个也不挂: {env}"
    );
    assert_eq!(
        batch_note_id(&pool, ready_a).await,
        None,
        "A 的 READY 批次必须仍是未挂单状态（不允许 per-part 先挂后验）"
    );
}

/// ★ 多个 part 全部凑不出 ⇒ message 是**多段**、按 `part_id` 升序、每段都带 part 身份。
///
/// 上一条钉的是「2 个 part、1 个失败」（`multiple == true` 但只有一段），本条覆盖另一侧：
/// `alloc_failure_error` 的 `sort_by_key(part_id)` 与 `segments.join("；")` 只在
/// `segments.len() >= 2` 时才真的被执行到。
///
/// 序是确定的：A 的 id 先于 B 生成（`pool_snowflake()` 同进程单调发号，见 `next_id` 的
/// doc），所以「A 段在前」就是「按 `part_id` 升序」—— 钉的是排序，不是请求里的插入序
/// （`targets` 是 `HashMap`，插入序不保证复现）。
#[tokio::test]
async fn multi_part_failures_are_listed_in_part_id_order() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "闸门多段").await;
    let l2 = insert_l2(&pool, "闸门多段二厂", l1).await;
    // A：只有 4 件可入单，本次要 8 件 ⇒ 必然凑不出；无状态明细 ⇒ 该段不带明细尾
    let part_a = insert_part(&pool, "多段甲件", "G-XORDA", l2).await;
    insert_batch(&pool, part_a, 1, 4, "READY_TO_SHIP", None).await;
    // B：4 件可入单 + 8 件 IN_PROCESS，本次要 8 件 ⇒ 必然凑不出；该段带状态明细尾
    let part_b = insert_part(&pool, "多段乙件", "G-XORDB", l2).await;
    insert_batch(&pool, part_b, 1, 8, "IN_PROCESS", None).await;
    insert_batch(&pool, part_b, 2, 4, "READY_TO_SHIP", None).await;
    assert!(
        part_a < part_b,
        "本用例的序前提不成立：A 的 id 应先于 B 生成（{part_a} vs {part_b}）"
    );

    let (s, env) = scan_entry(
        &app,
        &token,
        "G-XORDA",
        json!([
            {"node_kind": "PART", "node_id": part_a.to_string(), "quantity": 8},
            {"node_kind": "PART", "node_id": part_b.to_string(), "quantity": 8},
        ]),
    )
    .await;

    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "两个 part 都凑不出 ⇒ 整请求 21405: {env}"
    );
    assert_eq!(env["code"], 21405, "{env}");
    let msg = env["message"].as_str().unwrap();
    let a_pos = msg
        .find(&part_a.to_string())
        .expect("失败段必须带 A 的 part_id");
    let b_pos = msg
        .find(&part_b.to_string())
        .expect("失败段必须带 B 的 part_id");
    assert!(
        a_pos < b_pos,
        "多段必须按 part_id 升序拼接（A 段在前）: {msg}"
    );
    // 恰好两段：每个失败 part 一段，不重复也不合并
    assert_eq!(
        msg.matches("（需 8 件）：").count(),
        2,
        "每个失败 part 恰好一段（两个 target 都是 8 件）: {msg}"
    );
    assert!(
        msg.contains(&format!("part {part_a}（需 8 件）：可入单件数不足")),
        "A 的段应是「part 前缀 + DP 原文」，且不带状态明细尾: {msg}"
    );
    assert!(
        msg.contains("G-XORDB") && msg.contains("IN_PROCESS"),
        "B 段应追加被拦下的批次明细（serial_no + status）: {msg}"
    );
    assert!(
        !msg.contains("G-XORDA"),
        "A 没有非 READY 批次，明细段不该凭空造出它的批次: {msg}"
    );
    assert_eq!(
        attached_count(&pool).await,
        0,
        "两个 part 都失败 ⇒ 零写入: {env}"
    );
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
/// （`DRAFT` / `SUBMITTED` 才算）、状态又是 `READY_TO_SHIP`（不进 `not_ready_by_part`）⇒ 落进
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
