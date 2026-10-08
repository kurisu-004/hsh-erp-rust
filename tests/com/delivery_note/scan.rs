//! `POST /api/v2/com/delivery/note/scan` 扫码入单端到端测试（2026-10-08 重写）
//!
//! ## 与重写前的差异
//! 老实现是「扫一个码 → 自动把该件的全部 A 组批次（`INSPECTION` + `READY_TO_SHIP`）
//! 挂上 → 返回 `ScanOutcomeDto` 四态（ADDED / ALREADY_PRESENT /
//! CANDIDATES_AVAILABLE / PARTIAL_ADDED）+ 候选列表」。前端据此弹窗让用户二次勾选。
//!
//! 2026-10-08 重写为**单一入口 + 一次提交**：请求体带 `entries[]`（零件按件数、
//! 装配件按套数），服务端在同一事务内 find-or-create 草稿 + DP 分配 + 拆批 + 挂单 +
//! `note.version++`，出参是含拆批后完整行项的 `DeliveryNoteDetailOut`。旧的
//! 「候选弹窗 → 二次提交」两步走、5 组状态分类、`attach-batches` 端点全部删除。
//!
//! 本文件覆盖**成功路径**；闸门（21405 / 21406 / 21407）见 `entry_gate.rs`，
//! DP 分配算法见 `batch_allocation.rs`，扫码树见 `scan_tree.rs`。
//!
//! ## 覆盖
//! 1. 独立件：整批入单（target == batch.quantity，不拆批）
//! 2. 独立件：部分入单（target < batch.quantity ⇒ 拆批，原批次不动）
//! 3. 装配件：按套数整套入单（每个子件 target = sets × per_set）
//! 4. 装配件：`entry_max_sets` 闸门（sets 超上限 ⇒ 21405）
//! 5. 建单判定键单键：同 L1 第二次扫码**复用同一张草稿**（不新建）
//! 6. 送货单 OCC：`note_version` 不匹配 ⇒ 40901
//! 7. 未知序列号 ⇒ 404 / 21417
//! 8. 出参是 `DeliveryNoteDetailOut`（含 `line_items`），前端可就地替换草稿卡
//! 9. 行项默认序 = 加入本单的次序（`line_items[].delivery_seq`），不是批次 id；
//!    顺带覆盖摘单写点把 `delivery_seq` 清 NULL
//!
//! ## 测试栈
//! `tokio::test` + `test-support` 的 `send` / `json_request` / `login_token` /
//! `test_pool` / `test_state` / `test_app` / `load_delivery_fixture`。

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

// ===========================================================================
//  Bootstrap + fixtures
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, DeliveryFixture) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 直插 L1 客户（`serial_prefix` 传空串 ⇒ NULL，避免撞全局唯一索引）。
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

/// 直插 L2 客户（`serial_prefix` 必须为 NULL —— 只有 L1 有前缀）。
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

/// 直插工单（`assembly_id = Some(..)` 是装配件子件，`None` 是散件；`quantity` 是
/// **整单数量**，装配件下即「每套需要 整单数量/总套数 件」）。
async fn insert_part(
    pool: &PgPool,
    name: &str,
    serial_no: &str,
    customer_id: i64,
    assembly_id: Option<i64>,
    quantity: i32,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by, assembly_id) \
         VALUES ($1, $2, $3, 'D-SCAN', $4, 'READY_TO_SHIP', '扫描测试', $5, $5, $6, 0, \
         $7, NULL, $7, NULL, $8)",
    )
    .bind(id)
    .bind(Some(serial_no.to_string()))
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(quantity)
    .bind(now)
    .bind(assembly_id)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 直插装配件（`quantity` = 工单总套数）。
async fn insert_assembly(pool: &PgPool, name: &str, customer_id: i64, quantity: i32) -> i64 {
    let id = next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'D-ASM', $4, 'ACTIVE', '扫描测试', $5, $5, $6, 0, \
         $7, NULL, $7, NULL)",
    )
    .bind(id)
    .bind(Some(format!("ASM-{name}")))
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(quantity)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly");
    id
}

/// 直插一个 `READY_TO_SHIP` 且未占用的批次。
async fn insert_ready_batch(pool: &PgPool, part_id: i64, batch_no: i32, quantity: i32) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         delivery_note_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 'READY_TO_SHIP', 'PRODUCTION_SHELF', NULL, 0, $5, \
         NULL, $5, NULL)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(quantity)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert ready batch");
    id
}

/// 调 `POST /api/v2/com/delivery/note/scan`。
async fn scan_entry(
    app: &axum::Router,
    token: &str,
    serial_no: &str,
    note_version: Option<i32>,
    entries: Value,
) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/note/scan",
            Some(json!({
                "serial_no": serial_no,
                "note_version": note_version,
                "entries": entries,
            })),
            Some(token),
        ),
    )
    .await
}

/// 一个零件条目（雪花 id 必须是 JSON **string**）。
fn part_entry(part_id: i64, quantity: i32) -> Value {
    json!([{"node_kind": "PART", "node_id": part_id.to_string(), "quantity": quantity}])
}

/// 一个装配件条目（按套数）。
fn asm_entry(asm_id: i64, sets: i32) -> Value {
    json!([{"node_kind": "ASSEMBLY", "node_id": asm_id.to_string(), "sets": sets}])
}

/// 读 `line_items` 里某零件的行项。
fn line_of(data: &Value, part_id: i64) -> Value {
    data["line_items"]
        .as_array()
        .expect("line_items 必须是数组")
        .iter()
        .find(|li| li["part_id"].as_str() == Some(part_id.to_string().as_str()))
        .unwrap_or_else(|| panic!("行项里找不到 part {part_id}: {data}"))
        .clone()
}

// ===========================================================================
//  1~2. 独立件
// ===========================================================================

/// 整批入单：target == batch.quantity ⇒ 不拆批，行项就是原批次。
#[tokio::test]
async fn scan_entry_standalone_part_whole_batch_no_split() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "整批入单").await;
    let l2 = insert_l2(&pool, "整批二厂", l1).await;
    let part = insert_part(&pool, "整批件", "SB01", l2, None, 10).await;
    let batch = insert_ready_batch(&pool, part, 1, 6).await;

    let (s, env) = scan_entry(&app, &token, "SB01", None, part_entry(part, 6)).await;
    assert_eq!(s, StatusCode::OK, "整批入单应成功: {env}");
    assert_eq!(env["data"]["status"], "DRAFT");
    assert_eq!(env["data"]["line_items"].as_array().unwrap().len(), 1);
    let li = line_of(&env["data"], part);
    assert_eq!(li["id"].as_str().unwrap(), batch.to_string());
    assert_eq!(li["quantity"], 6);

    // 原批次只是被挂上单，没有被拆出新批次
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL",
    )
    .bind(part)
    .fetch_one(&pool)
    .await
    .expect("count batches");
    assert_eq!(n, 1, "整批入单不应产生新批次");
}

/// 部分入单：target < batch.quantity ⇒ **拆批**，差额挂单。
///
/// 这是 DP 的「拆 1 个最大批次」路径的端到端验证。语义照
/// `PartBatchRepo::split_batch`：新批次 `quantity = qty`、**不继承**
/// `delivery_note_id`（随后由入单路径挂上）；源批次 `quantity -= qty`、保持未挂单。
#[tokio::test]
async fn scan_entry_partial_quantity_splits_batch() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "拆批入单").await;
    let l2 = insert_l2(&pool, "拆批二厂", l1).await;
    let part = insert_part(&pool, "拆批件", "SB02", l2, None, 10).await;
    let batch = insert_ready_batch(&pool, part, 1, 10).await;

    let (s, env) = scan_entry(&app, &token, "SB02", None, part_entry(part, 4)).await;
    assert_eq!(s, StatusCode::OK, "部分入单应成功: {env}");
    let li = line_of(&env["data"], part);
    assert_eq!(li["quantity"], 4, "入单 4 件");

    // 新批次 = 差额 4；原批次 10 件仍在但**未挂单**
    let rows: Vec<(i64, i32, Option<i64>)> = sqlx::query_as(
        "SELECT id, quantity, delivery_note_id FROM t_part_batch \
         WHERE part_id = $1 AND deleted_at IS NULL ORDER BY id",
    )
    .bind(part)
    .fetch_all(&pool)
    .await
    .expect("list batches");
    assert_eq!(rows.len(), 2, "应拆出新批次: {rows:?}");
    let new_id = li["id"].as_str().unwrap().to_string();
    let original = rows
        .iter()
        .find(|r| r.0.to_string() == batch.to_string())
        .unwrap();
    assert_eq!(
        original.1, 6,
        "拆批是「源批次减量 + 新批次增量」，源批次数量从 10 变 6"
    );
    assert!(
        original.2.is_none(),
        "原批次不该被挂单（只挂差额那一批）: {:?}",
        original
    );
    let split = rows.iter().find(|r| r.0.to_string() == new_id).unwrap();
    assert_eq!(split.1, 4);
    assert!(split.2.is_some(), "拆出的批次应挂单");
}

// ===========================================================================
//  3~4. 装配件
// ===========================================================================

/// 装配件按套数整套入单：每个子件 target = sets × (part.quantity /
/// assembly.quantity)（整数除法向零截断），子件之间各跑一次 DP。
///
/// 数据：装配件 10 套；子件 A 整单 20 件（每套 2 件）、子件 B 整单 10 件（每套 1 件）。
/// 送 3 套 ⇒ A 入 6 件、B 入 3 件，各整批命中（batch 恰好 6 / 3）。
#[tokio::test]
async fn scan_entry_assembly_expands_sets_to_each_child() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "装配套数").await;
    let l2 = insert_l2(&pool, "装配套数二厂", l1).await;
    let asm = insert_assembly(&pool, "套装", l2, 10).await;
    let child_a = insert_part(&pool, "套件A", "SA01", l2, Some(asm), 20).await;
    let child_b = insert_part(&pool, "套件B", "SA02", l2, Some(asm), 10).await;
    insert_ready_batch(&pool, child_a, 1, 6).await;
    insert_ready_batch(&pool, child_b, 1, 3).await;

    let (s, env) = scan_entry(&app, &token, "SA01", None, asm_entry(asm, 3)).await;
    assert_eq!(s, StatusCode::OK, "装配套数入单应成功: {env}");
    assert_eq!(
        line_of(&env["data"], child_a)["quantity"],
        6,
        "A：3 套 × 2 件"
    );
    assert_eq!(
        line_of(&env["data"], child_b)["quantity"],
        3,
        "B：3 套 × 1 件"
    );
    let li_a = line_of(&env["data"], child_a);
    assert_eq!(
        li_a["shippable_sets"], 3,
        "详情 VO 的 shippable_sets 应等于本次套数"
    );
}

/// `sets` 超过可组套数上限 ⇒ 21405（不挂任何批次、单据 version 不变）。
///
/// 数据：装配件 10 套；子件 A 整单 10 件但只有 2 件 READY_TO_SHIP ⇒ 最多 2 套。
#[tokio::test]
async fn scan_entry_assembly_sets_over_cap_returns_21405() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "套数上限").await;
    let l2 = insert_l2(&pool, "套数上限二厂", l1).await;
    let asm = insert_assembly(&pool, "上限套装", l2, 10).await;
    let child_a = insert_part(&pool, "上限件A", "SA11", l2, Some(asm), 10).await;
    insert_ready_batch(&pool, child_a, 1, 2).await;

    let (s, env) = scan_entry(&app, &token, "SA11", None, asm_entry(asm, 5)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "超上限应拒: {env}");
    assert_eq!(env["code"], 21405);
    let attached: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_part_batch WHERE delivery_note_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .expect("count attached");
    assert_eq!(
        attached, 0,
        "闸门拒绝时不应有批次被挂上（整个请求在同一事务里回滚）"
    );
}

// ===========================================================================
//  5~8. 建单判定键 / OCC / 未命中 / 出参形态
// ===========================================================================

/// 建单判定键是单键 `(customer_id, status='DRAFT')`：同 L1 第二次扫码**复用同一张
/// 草稿**（不新建），且两次 entry 累加到同一张单上。
#[tokio::test]
async fn scan_entry_same_l1_reuses_single_draft() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "单键建单").await;
    let l2 = insert_l2(&pool, "单键建单二厂", l1).await;
    let p1 = insert_part(&pool, "单键件1", "SK01", l2, None, 10).await;
    let p2 = insert_part(&pool, "单键件2", "SK02", l2, None, 10).await;
    insert_ready_batch(&pool, p1, 1, 5).await;
    insert_ready_batch(&pool, p2, 1, 5).await;

    let (s1, env1) = scan_entry(&app, &token, "SK01", None, part_entry(p1, 5)).await;
    assert_eq!(s1, StatusCode::OK, "第一次扫码: {env1}");
    let note_id = env1["data"]["id"].as_str().unwrap().to_string();
    let v1 = env1["data"]["version"].as_i64().unwrap();

    // 第二次：换零件、note_version 带上第一次返回的 version
    let (s2, env2) = scan_entry(&app, &token, "SK02", Some(v1 as i32), part_entry(p2, 5)).await;
    assert_eq!(s2, StatusCode::OK, "第二次扫码: {env2}");
    assert_eq!(
        env2["data"]["id"].as_str().unwrap(),
        note_id,
        "同 L1 必须复用同一张 DRAFT（判定键单键）"
    );
    assert_eq!(
        env2["data"]["line_items"].as_array().unwrap().len(),
        2,
        "两次入单的行项应累加在同一张单上"
    );

    let drafts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_delivery_note WHERE customer_id = $1 AND status = 'DRAFT' AND deleted_at IS NULL")
            .bind(l1)
            .fetch_one(&pool)
            .await
            .expect("count drafts");
    assert_eq!(drafts, 1, "同 L1 只能有一张 DRAFT");
}

/// 送货单 OCC：`note_version` 与库里的不一致 ⇒ 40901，且不挂任何批次。
#[tokio::test]
async fn scan_entry_note_version_mismatch_returns_40901() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "OCC 入单").await;
    let l2 = insert_l2(&pool, "OCC 二厂", l1).await;
    let part = insert_part(&pool, "OCC 件", "SK11", l2, None, 10).await;
    insert_ready_batch(&pool, part, 1, 5).await;

    let (s, env) = scan_entry(&app, &token, "SK11", Some(999), part_entry(part, 5)).await;
    assert_eq!(s, StatusCode::CONFLICT, "OCC 不匹配应 409: {env}");
    assert_eq!(env["code"], 40901);
    let attached: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_part_batch WHERE delivery_note_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(attached, 0, "OCC 失败必须零写入");
}

/// 未知序列号 ⇒ 404 / 21417，且**不建单**（建单发生在解析之后、且必须先解析成功）。
#[tokio::test]
async fn scan_entry_unknown_serial_returns_404_21417_and_creates_no_note() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM t_delivery_note")
        .fetch_one(&pool)
        .await
        .expect("count notes");

    let (s, env) = scan_entry(&app, &token, "NOPE-999", None, part_entry(1, 1)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "未知码应 404: {env}");
    assert_eq!(env["code"], 21417);

    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM t_delivery_note")
        .fetch_one(&pool)
        .await
        .expect("count notes");
    assert_eq!(after, before, "扫到不存在的码不该凭空建单");
}

/// 空 `serial_no` / 空 `entries` ⇒ 400（`BIZ_INVALID_VALUE`）。
#[tokio::test]
async fn scan_entry_blank_serial_or_empty_entries_returns_400() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    // `AppError::validation` 走 422（错误码 40001 BIZ_INVALID_VALUE）—— 与全仓
    // 入参校验一致，不是 400。
    let (s1, env1) = scan_entry(&app, &token, "   ", None, part_entry(1, 1)).await;
    assert_eq!(
        s1,
        StatusCode::UNPROCESSABLE_ENTITY,
        "空白 serial_no: {env1}"
    );

    let (s2, env2) = scan_entry(&app, &token, "SB01", None, json!([])).await;
    assert_eq!(s2, StatusCode::UNPROCESSABLE_ENTITY, "空 entries: {env2}");
}

/// 出参是 `DeliveryNoteDetailOut`（head 扁平 + `line_items`），前端可就地替换草稿卡
/// —— 钉死「回传的是完整详情而不是 `{id, version}` 这种摘要」。
#[tokio::test]
async fn scan_entry_returns_full_detail_so_frontend_can_replace_draft_card() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "出参形态").await;
    let l2 = insert_l2(&pool, "出参形态二厂", l1).await;
    let part = insert_part(&pool, "出参件", "SR01", l2, None, 10).await;
    insert_ready_batch(&pool, part, 1, 3).await;

    let (s, env) = scan_entry(&app, &token, "SR01", None, part_entry(part, 3)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let d = &env["data"];
    // head 字段（DeliveryNoteOut 被 flatten 到顶层）
    for key in ["id", "delivery_note_no", "status", "version", "part_count"] {
        assert!(d.get(key).is_some(), "出参缺 head 字段 {key}: {env}");
    }
    assert!(d["line_items"].is_array(), "出参必须带 line_items: {env}");
    assert_eq!(d["line_items"][0]["quantity"], 3);
    assert!(
        d.get("scanned_serials").is_none(),
        "2026-10-08：`scanned_serials` 已从 VO 删除"
    );
}

/// 2026-10-10 新增：行项默认序 = **加入本单的次序**，不是批次 id（建批次序）。
///
/// 场景刻意把两者反向：先建的批次（id 小）**后扫码**，后建的批次（id 大）**先扫码**。
/// 排序键若仍是 `pb.id ASC`，`line_items[0]` 会是后扫的那个 —— 与用户看到的
/// 扫码次序相反。`delivery_seq` 钉死正确口径。
///
/// 顺带覆盖摘单写点：`remove-batches` 把批次移出本单时 `delivery_seq` 归 NULL，
/// 重新挂到别单时会按新单重新计数，不带旧序号。
#[tokio::test]
async fn scan_entry_orders_line_items_by_delivery_seq_not_batch_id() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "挂单次序").await;
    let l2 = insert_l2(&pool, "挂单次序二厂", l1).await;
    let part_scanned_first = insert_part(&pool, "先扫件", "SQ01", l2, None, 10).await;
    let part_scanned_second = insert_part(&pool, "后扫件", "SQ02", l2, None, 10).await;
    // ⚠️ 建批顺序与扫码顺序**故意相反**：后扫件的批次 id 更小
    let batch_second = insert_ready_batch(&pool, part_scanned_second, 1, 5).await;
    let batch_first = insert_ready_batch(&pool, part_scanned_first, 1, 5).await;
    assert!(
        batch_second < batch_first,
        "前置条件：后扫的批次 id 必须更小，否则本用例测不出排序键切换"
    );

    let (s1, env1) = scan_entry(
        &app,
        &token,
        "SQ01",
        None,
        part_entry(part_scanned_first, 5),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "第一次扫码: {env1}");
    let v1 = env1["data"]["version"].as_i64().unwrap();

    let (s2, env2) = scan_entry(
        &app,
        &token,
        "SQ02",
        Some(v1 as i32),
        part_entry(part_scanned_second, 5),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "第二次扫码: {env2}");

    let items = env2["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0]["part_id"].as_str().unwrap(),
        part_scanned_first.to_string(),
        "行项默认序必须按加入本单的次序，不是批次 id: {env2}"
    );
    assert_eq!(items[0]["delivery_seq"], 1);
    assert_eq!(
        items[1]["part_id"].as_str().unwrap(),
        part_scanned_second.to_string()
    );
    assert_eq!(items[1]["delivery_seq"], 2);

    // 摘单 ⇒ delivery_seq 归 NULL（与 delivery_note_id 同点清）
    let note_id = env2["data"]["id"].as_str().unwrap().to_string();
    let v2 = env2["data"]["version"].as_i64().unwrap();
    let (s3, env3) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/note/{note_id}/remove-batches"),
            Some(json!({
                "batch_ids": [batch_first.to_string()],
                "version": v2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::OK, "摘单应成功: {env3}");
    let seq_left: Option<i64> =
        sqlx::query_scalar("SELECT delivery_seq FROM t_part_batch WHERE id = $1")
            .bind(batch_first)
            .fetch_one(&pool)
            .await
            .expect("read delivery_seq");
    assert_eq!(
        seq_left, None,
        "摘单必须清 delivery_seq，否则该批次改挂别单时会带旧序号"
    );
}
