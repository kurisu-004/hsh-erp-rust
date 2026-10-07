//! 采购订单 Excel 导入端点集成测试（2026-10-06 新增）
//!
//! 覆盖 `POST /parts/match-by-excel-items`（Excel 行 → 候选零件）与
//! `POST /parts/batch-update-order-info`（批量回填订单号 / 系统交期）——
//! 这两条端点此前**零覆盖**，且契约长期与前端不一致（前端按 `row_no` 关联并读
//! `parts` 数组，后端曾返扁平 `{part_id, status}` 数组 ⇒ 每行都判「未匹配」）。
//!
//! ## 场景清单（括号内是任务书编号，便于对照）
//!
//! 1. `match_part_code_returns_existing_values`（**1**）—— PART_CODE 命中，且
//!    `version` / `order_no` / `system_delivery_date` / `drawing_no` / `name`
//!    **逐字段等于 DB 值**（前端 OCC 提交 + 「现值对比」的数据源，错了会误勾选）；
//! 2. `match_part_code_multiple_candidates_keeps_all`（**2**）—— 同图号 2 行 ⇒ 2
//!    候选，且顺序必须可断言（图号档 id DESC）；
//! 3. `match_part_name_fallback_when_code_missing`（**3**）—— 图号不存在 / 名称命中
//!    `t_part.name` ⇒ PART_NAME；
//! 4. `match_assembly_code_returns_active_children`（**4**）—— 装配件图号命中 ⇒
//!    候选是**子件**（带 `assembly_name`），软删子件不参与，装配件本身不入候选；
//! 5. `match_none_row_still_present_with_empty_array`（**5**）—— 未匹配行仍在结果
//!    里，`match_type = "NONE"` 且 `parts` 是**空数组**（不是 null / 不是缺字段）；
//! 6. `match_soft_deleted_rows_excluded`（**6**）—— 软删的零件 / 装配件都不参与；
//! 7. `match_result_length_and_row_no_align_with_items`（**7**）—— 3 行请求（命中 1
//!    / 命中 2 / 未命中）⇒ 长度恒 3 且 row_no 逐个对应；
//! 8. `match_rejects_inspector`（**8**）—— INSPECTOR ⇒ 403 + 40300；
//! 9. `match_rejects_empty_items`（**9**）—— `items: []` ⇒ 422 + 40001；
//! 10. `update_two_parts_writes_and_bumps_version`（**10**）—— 2 件成功 + **回读 DB**
//!     确认 `order_no` / `system_delivery_date` 真落库、`version` 真 +1；
//! 11. `update_version_conflict_lands_in_failed_with_http_200`（**11**）—— OCC 冲突
//!     ⇒ `updated_count = 0` + `failed[].code = 40901`，HTTP 仍 200，DB 未变；
//! 12. `update_partial_success_isolates_bad_row`（**12**）—— 3 条中 1 条版本错 ⇒
//!     `updated_count = 2` / `failed.len() = 1`（不会一错全错）；
//! 13. `update_tristate_clears_column_on_explicit_null`（**13a**）与
//!     `update_tristate_absent_field_leaves_column_untouched`（**13b**）—— 三态：显式
//!     `null` ⇒ 该列**真的**变 NULL（旧单层 `Option` 语义下清空是静默无效的）；字段
//!     **缺省** ⇒ 该列保持原值（与「显式 null」是两个不同的态）；
//! 14. `update_skip_row_does_not_touch_db`（**14**）—— `skip: true` ⇒ 不写库、计入
//!     `skipped_count`、既不进 updated 也不进 failed；
//! 15. `match_rejects_items_over_limit` —— 超 2000 行 ⇒ 422 + 40001；
//! 16. `match_ignores_unknown_item_fields` —— 前端仍在发 `doc_no` / `delivery_date`
//!     / `unit_price` / `quantity`，后端不声明它们，必须照常 200（serde 默认忽略未知
//!     字段；反过来若哪天声明成 `Option<NaiveDate>`，`delivery_date` 的非日期文本会把
//!     **整个请求**打成 400）；
//! 17. `update_failure_message_has_no_sqlx_internals` —— DB 错误行的 `failed[].message`
//!     不得含 sqlx 原始文本（细节走 `tracing::warn!` 收口，响应体只给中文文案）；
//! 18. `update_rejects_inspector` / `update_rejects_empty_items` —— 两个端点的权限
//!     与入参闸门。
//!
//! ## review 第 1 轮新增（2026-10-06）
//!
//! 19. `update_invalid_date_text_fails_only_that_row` —— **MAJOR-1**：`system_delivery_date`
//!     给非日期文本（`2026年8月1日` / `待定`，正是前端 `parseDateOrNull` 原样透传的那类值）
//!     ⇒ 该行进 `failed[]`（40001）+ **HTTP 仍 200**，其余行照常写库。改动前该文本在
//!     axum JsonRejection 层就 400（纯文本 body、不是信封）⇒ 整批回填全部作废；
//! 20. `update_valid_date_with_surrounding_space_still_writes` —— MAJOR-1 的**向后兼容**
//!     一侧：合法日期（含两端空白）照常解析落库、version +1；
//! 21. `update_soft_deleted_part_is_not_backfilled` —— **MINOR-4**：软删的 part 不被回填
//!     （`updated_count = 0` + 40901，且 `deleted_at` / `version` / `order_no` 均未变）；
//! 22. `update_rejects_items_over_limit` —— **MINOR-5**：超 2000 行 ⇒ 422 + 40001，且
//!     **不入循环**（入参里那 2001 行的目标行在 DB 上必须原封不动）。
//!
//! ## review 第 3 轮新增（2026-10-06）
//!
//! 23. `update_order_no_too_long_fails_only_that_row` —— **R3-1**：`order_no` 超
//!     `varchar(30)` ⇒ 行级 40001 + 该行 DB 完全未动（`order_no` / `version` 都没变），
//!     其余行照常写。触发路径现实可达：前端把采购订单「单据编号」原文（无长度校验、
//!     无截断）作为**每一行**的默认 `orderNo` ⇒ 一张 PO 超 30 字就整批全撞；
//! 24. `update_note_too_long_fails_only_that_row` —— R3-1：`note` 超 `varchar(500)`
//!     同样行级 40001（与 `order_no` 同构但列不同，各自锁一次）；
//! 25. `update_length_gate_counts_chars_not_bytes` —— R3-1 的口径边界：按 **char** 计
//!     （30 个汉字 = 90 字节必须放行）、按**原长度**判（不 trim：带空白的 36 字也拒，
//!     防「trim 后计数 / 原串写库」那个会漏成 50001 的洞）；
//! 26. `update_skip_row_does_not_touch_db` 增补 —— **R3-8**：skip 那行同时踩三道闸门
//!     （version 错 + 非日期交期 + 超长订单号），`failed` 仍须为空。
//!
//! ## 基建
//! 沿用 `crud.rs` / `create_serial_price.rs` 的写法（`test_pool` +
//! `load_part_fixture` + `send` / `json_request` / `login_token`），不新建 helper
//! 体系、不本地重声明 `send` / `json_request` / `login_*` / `insert_*`。
//! fixture **刻意不 seed 任何 `t_part` / `t_assembly` 行**（见
//! `test-support/src/fixture/part.rs` 的说明），故本文件自建两个 insert helper。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::*;

// ===========================================================================
//  本文件私有 helper（与 crud.rs / create_serial_price.rs 同风格：sub-file 私有）
// ===========================================================================

/// 取下一个测试雪花 ID。
///
/// 2026-10-09：本文件原有一个私有 `po_snowflake()` —— `OnceLock<SnowflakeIdGenerator>`
/// 塞 instance=`777` 的域内单例，doc 里写「单独一个 instance 段，避免与其它 sub-file
/// 在同一测试库里撞 id」。instance 不同确实位段不同、不会撞，但它是**次优权宜之计**：
/// instance 仅 10 bit = 1024 槽，字面量仍有与本进程 `test_snowflake_instance()` 派生值
/// 相等的 1/1024 概率，且一旦相等、两个 fresh generator 的首个 id 同为 `seq=0` 就撞
/// `t_part_pkey`（23505）。现统一走全进程共享 generator：共享对象按 `next_id()`
/// 调用顺序串行发号，进程内天然唯一，不再需要靠 instance 制造区分度。
fn next_id() -> i64 {
    hsh_erp_test_support::shared_test_snowflake().next_id()
}

/// 插入一行 `t_part`（匹配链路读到的列全部参数化）。
///
/// `drawing_no` 是 `NOT NULL VARCHAR(100)`，故本 helper 要求显式传入（不提供
/// 默认值）——「图号缺失」这个形态在 `t_part` 上根本不存在，测试也不该造。
#[allow(clippy::too_many_arguments)]
async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    drawing_no: &str,
    name: &str,
    assembly_id: Option<i64>,
    order_no: Option<&str>,
    system_delivery_date: Option<chrono::NaiveDate>,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, customer_id, assembly_id, status, \
         version, order_no, system_delivery_date, created_at, updated_at) \
         VALUES ($1, $2, $3, '甲', 1, $4, $4, $5, $6, 'PENDING', 0, $7, $8, $9, $9)",
    )
    .bind(id)
    .bind(name)
    .bind(drawing_no)
    .bind(today)
    .bind(customer_id)
    .bind(assembly_id)
    .bind(order_no)
    .bind(system_delivery_date)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 插入一行 `t_assembly`（`unit_price` / `total_price` 是 NOT NULL DEFAULT 0，
/// 显式给 0 与 `tests/assembly/status_sync.rs` 同款，避免依赖 DEFAULT）。
async fn insert_assembly(pool: &PgPool, customer_id: i64, drawing_no: &str, name: &str) -> i64 {
    let id = next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, \
         total_price, version, created_at, updated_at) \
         VALUES ($1, $2, $3, '甲', $4, $5, $5, 'PENDING', 1, 0, 0, 0, $6, $6)",
    )
    .bind(id)
    .bind(drawing_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly");
    id
}

/// 软删一行 `t_part`（`deleted_at` 非空 ⇒ 匹配链路不应命中）。
async fn soft_delete_part(pool: &PgPool, part_id: i64) {
    sqlx::query("UPDATE t_part SET deleted_at = now(), version = version + 1 WHERE id = $1")
        .bind(part_id)
        .execute(pool)
        .await
        .expect("soft delete part");
}

/// 软删一行 `t_assembly`。
async fn soft_delete_assembly(pool: &PgPool, assembly_id: i64) {
    sqlx::query("UPDATE t_assembly SET deleted_at = now(), version = version + 1 WHERE id = $1")
        .bind(assembly_id)
        .execute(pool)
        .await
        .expect("soft delete assembly");
}

/// 回读 `t_part` 的「现值三件套」（`version` / `order_no` / `system_delivery_date`）。
async fn read_order_info(
    pool: &PgPool,
    part_id: i64,
) -> (i32, Option<String>, Option<chrono::NaiveDate>) {
    sqlx::query_as::<_, (i32, Option<String>, Option<chrono::NaiveDate>)>(
        "SELECT version, order_no, system_delivery_date FROM t_part WHERE id = $1",
    )
    .bind(part_id)
    .fetch_one(pool)
    .await
    .expect("read order info")
}

/// 调 `POST /parts/match-by-excel-items` 并返回 `(HTTP 状态, 信封)`。
///
/// `doc_no` 参数：前端 `PartBatchOrderInfoMatchRequest` 的**顶层**也带 `doc_no`
///（采购订单号），而后端 `MatchByExcelItemsRequest` 只有 `items` —— 顶层未知字段
/// 被 serde 默认忽略是契约的一部分，故本 helper 把它一起发出去，让每个 match
/// 用例都覆盖这条（而不是只在专用用例里发一次）。
async fn call_match(app: axum::Router, token: &str, items: Value) -> (StatusCode, Value) {
    send(
        app,
        json_request(
            "POST",
            "/parts/match-by-excel-items",
            Some(json!({ "doc_no": "PO-2026-0001", "items": items })),
            Some(token),
        ),
    )
    .await
}

/// 调 `POST /parts/batch-update-order-info` 并返回 `(HTTP 状态, 信封)`。
async fn call_update(app: axum::Router, token: &str, items: Value) -> (StatusCode, Value) {
    send(
        app,
        json_request(
            "POST",
            "/parts/batch-update-order-info",
            Some(json!({ "items": items })),
            Some(token),
        ),
    )
    .await
}

/// 取结果数组第 `i` 行（带「第 i 行必须存在」的前置断言）。
fn row(data: &Value, i: usize) -> &Value {
    data.as_array()
        .unwrap_or_else(|| panic!("data 应是数组: {data}"))
        .get(i)
        .unwrap_or_else(|| panic!("结果数组缺第 {i} 行: {data}"))
}

/// 断言某行的 `parts` 非空并返回其中第 `j` 个候选。
fn part_at(data: &Value, i: usize, j: usize) -> &Value {
    row(data, i)["parts"]
        .as_array()
        .unwrap_or_else(|| panic!("第 {i} 行 parts 应是数组: {}", row(data, i)))
        .get(j)
        .unwrap_or_else(|| panic!("第 {i} 行缺第 {j} 个候选: {}", row(data, i)))
}

/// 断言 `failed[].message` 是干净的中文文案（不含 sqlx 内部错误文本）。
///
/// 2026-10-06：旧实现把 `format!("{e}")`（sqlx 的 `Database(22P02)` + 列名 + 约束名）
/// 直接塞进 200 响应体。这里逐条钉死「不能泄 internals」，并顺带钉死 message
/// **非空**（空串会让前端弹一个没有信息量的红框）。
fn assert_failure_message_clean(failure: &Value, ctx: &str) {
    let msg = failure["message"]
        .as_str()
        .unwrap_or_else(|| panic!("{ctx}: failed[].message 应是字符串: {failure}"));
    assert!(!msg.trim().is_empty(), "{ctx}: message 不得为空: {failure}");
    for leak in [
        "sqlx",
        "SQLx",
        "database error",
        "Database",
        "column",
        "constraint",
        "pg",
        "ERROR",
    ] {
        assert!(
            !msg.contains(leak),
            "{ctx}: message 泄出了 internals（命中 {leak:?}）: {msg:?}"
        );
    }
}

// ===========================================================================
//  bootstrap helpers
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  match 端点：PART_CODE 命中（现值逐字段回显）
// ===========================================================================

/// 场景 1：`t_part` 有一行 `drawing_no='PO-001'` ⇒ PART_CODE，候选 1，
/// 且候选的 5 个「现值字段」逐字段等于 DB 值。
///
/// 这几个字段是前端**提交时的数据源**（`version` 是 OCC 锚，`order_no` /
/// `system_delivery_date` 是「原值 vs 新值」对比的左侧），任一个错了都会让用户
/// 在对话框里看到错误现值、或提交后撞 409。
#[tokio::test]
async fn match_part_code_returns_existing_values() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let sys_date = chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-001",
        "法兰盘",
        None,
        Some("PO-OLD-1"),
        Some(sys_date),
    )
    .await;
    // version 从 0 抬到 3，模拟「这行被人改过几轮」——OCC 锚必须是当前值
    sqlx::query("UPDATE t_part SET version = 3 WHERE id = $1")
        .bind(pid)
        .execute(&pool)
        .await
        .expect("bump version");

    let (s, env) = call_match(
        app,
        &token,
        json!([{ "row_no": 5, "line_no": "10", "drawing_no": "PO-001", "name": "任意描述" }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "match: {env}");
    assert_eq!(env["code"], 0, "{env}");
    let data = &env["data"];
    assert_eq!(data.as_array().unwrap().len(), 1, "{env}");
    let r = row(data, 0);
    assert_eq!(r["row_no"], 5, "row_no 应回显: {r}");
    assert_eq!(r["match_type"], "PART_CODE", "{r}");
    assert!(
        r["warnings"].as_array().is_some_and(|w| w.is_empty()),
        "单候选不该有 warning: {r}"
    );
    let p = part_at(data, 0, 0);
    // 雪花 id 序列化成 JSON string（不是数字）
    assert_eq!(
        p["part_id"].as_str(),
        Some(pid.to_string().as_str()),
        "part_id 应是雪花 id 的字符串形态: {p}"
    );
    assert_eq!(p["version"], 3, "version 必须回传 OCC 锚: {p}");
    assert_eq!(p["drawing_no"], "PO-001", "{p}");
    assert_eq!(p["name"], "法兰盘", "{p}");
    assert_eq!(p["order_no"], "PO-OLD-1", "现值必须回传: {p}");
    assert_eq!(
        p["system_delivery_date"], "2026-11-20",
        "现值必须回传（YYYY-MM-DD）: {p}"
    );
    assert!(p["assembly_id"].is_null(), "无所属装配件: {p}");
    assert!(p["assembly_name"].is_null(), "无所属装配件: {p}");
}

// ===========================================================================
//  match 端点：同图号多候选（cap 以内全给，顺序可断言）
// ===========================================================================

/// 场景 2：两条 `t_part` 同 `drawing_no` ⇒ 2 候选都在，且图号档按 id DESC
/// （最近建的排前，沿用旧实现口径），cap 20 以内不产生截断 warning。
#[tokio::test]
async fn match_part_code_multiple_candidates_keeps_all() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let first = insert_part(&pool, fx.customer_l2_id, "PO-DUP", "件甲", None, None, None).await;
    let second = insert_part(&pool, fx.customer_l2_id, "PO-DUP", "件乙", None, None, None).await;

    let (s, env) = call_match(
        app,
        &token,
        json!([{ "row_no": 7, "drawing_no": "PO-DUP" }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let data = &env["data"];
    let r = row(data, 0);
    assert_eq!(r["match_type"], "PART_CODE", "{r}");
    let parts = r["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2, "两条命中都应作为候选: {r}");
    assert_eq!(
        parts[0]["part_id"].as_str(),
        Some(second.to_string().as_str()),
        "图号档应 id DESC（后建的排前）: {r}"
    );
    assert_eq!(
        parts[1]["part_id"].as_str(),
        Some(first.to_string().as_str())
    );
    assert!(
        r["warnings"].as_array().is_some_and(|w| w.is_empty()),
        "2 < cap=20 不该报截断: {r}"
    );
}

// ===========================================================================
//  match 端点：名称兜底 PART_NAME
// ===========================================================================

/// 场景 3：图号不存在、`name` 命中 `t_part.name` ⇒ PART_NAME。
#[tokio::test]
async fn match_part_name_fallback_when_code_missing() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "D-NAME-1",
        "同步带",
        None,
        None,
        None,
    )
    .await;

    let (s, env) = call_match(
        app,
        &token,
        json!([{ "row_no": 9, "drawing_no": "PO-查无此件", "name": "同步带" }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let r = row(&env["data"], 0);
    assert_eq!(r["match_type"], "PART_NAME", "图号落空应退到名称档: {r}");
    assert_eq!(
        part_at(&env["data"], 0, 0)["part_id"].as_str(),
        Some(pid.to_string().as_str())
    );
}

// ===========================================================================
//  match 端点：装配件图号 ASSEMBLY_CODE（候选是子件 + assembly_name）
// ===========================================================================

/// 场景 4：装配件图号命中 ⇒ 候选是该装配件的**有效子件**（不是装配件本身），
/// 且每个候选都带 `assembly_name`；软删子件不参与。
#[tokio::test]
async fn match_assembly_code_returns_active_children() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let asm_id = insert_assembly(&pool, fx.customer_l2_id, "PO-ASM", "法兰装配体").await;
    let child1 = insert_part(
        &pool,
        fx.customer_l2_id,
        "C-1",
        "子件一",
        Some(asm_id),
        None,
        None,
    )
    .await;
    let child2 = insert_part(
        &pool,
        fx.customer_l2_id,
        "C-2",
        "子件二",
        Some(asm_id),
        None,
        None,
    )
    .await;
    let child_soft = insert_part(
        &pool,
        fx.customer_l2_id,
        "C-3",
        "子件三",
        Some(asm_id),
        None,
        None,
    )
    .await;
    soft_delete_part(&pool, child_soft).await;

    let (s, env) = call_match(
        app,
        &token,
        json!([{ "row_no": 11, "drawing_no": "PO-ASM" }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let data = &env["data"];
    let r = row(data, 0);
    assert_eq!(r["match_type"], "ASSEMBLY_CODE", "{r}");
    let parts = r["parts"].as_array().unwrap();
    assert_eq!(
        parts.len(),
        2,
        "应恰好返回 2 个有效子件（软删子件被 SQL 层 deleted_at 闸门滤掉）: {r}"
    );
    let mut ids: Vec<String> = parts
        .iter()
        .map(|p| p["part_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    let mut want = vec![child1.to_string(), child2.to_string()];
    want.sort();
    assert_eq!(ids, want, "候选应是子件: {r}");
    assert!(
        !ids.contains(&child_soft.to_string()),
        "软删子件不得进候选: {r}"
    );
    assert!(
        !ids.contains(&asm_id.to_string()),
        "装配件本身绝不入候选（它不在 t_part，batch-update 打不到它）: {r}"
    );
    for p in parts {
        assert_eq!(
            p["assembly_name"], "法兰装配体",
            "候选应带所属装配件名: {p}"
        );
        assert_eq!(
            p["assembly_id"].as_str(),
            Some(asm_id.to_string().as_str()),
            "assembly_id 应是字符串形态: {p}"
        );
    }
}

// ===========================================================================
//  match 端点：未匹配行仍出现且 parts 是空数组
// ===========================================================================

/// 场景 5：完全不存在的物料代码 ⇒ 结果里**仍有这一行**、`match_type = "NONE"`、
/// `parts` 是空数组（不是 null、不是缺字段）。
///
/// 长度守恒是前端 `new Map(results.map(r => [r.row_no, r]))` 能建起来的前提：
/// 少一行 ⇒ 该 Excel 行在对话框里整行消失。
#[tokio::test]
async fn match_none_row_still_present_with_empty_array() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = call_match(
        app,
        &token,
        json!([{ "row_no": 21, "drawing_no": "PO-完全不存在" }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let r = row(&env["data"], 0);
    assert_eq!(r["row_no"], 21, "{r}");
    assert_eq!(r["match_type"], "NONE", "{r}");
    assert!(
        r["parts"].as_array().is_some_and(|p| p.is_empty()),
        "NONE 档的 parts 必须是空数组而非 null / 缺字段: {r}"
    );
    assert!(
        r["warnings"].as_array().is_some_and(|w| w.is_empty()),
        "「没匹配上」不是异常，不该 warning: {r}"
    );
}

// ===========================================================================
//  match 端点：软删不参与
// ===========================================================================

/// 场景 6：图号命中的零件 / 装配件都已软删 ⇒ 两档都落空 ⇒ NONE。
///
/// 第 3 行额外锁住一条当前口径：**软删闸门按行判定，不向下级联** —— 子件行本身是
/// active 的（它没被软删），所以仍能被自己的图号命中。哪天产品决定改成「装配件软删
/// ⇒ 其子件一并视为不可用」，这条断言要同步改（那是行为变更，不是测试修正）。
#[tokio::test]
async fn match_soft_deleted_rows_excluded() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let soft_part = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-SOFT",
        "软删件",
        None,
        None,
        None,
    )
    .await;
    soft_delete_part(&pool, soft_part).await;
    let asm = insert_assembly(&pool, fx.customer_l2_id, "PO-SOFT-ASM", "软删装配").await;
    let child = insert_part(
        &pool,
        fx.customer_l2_id,
        "C-SOFT",
        "子件",
        Some(asm),
        None,
        None,
    )
    .await;
    soft_delete_assembly(&pool, asm).await;

    let (s, env) = call_match(
        app,
        &token,
        json!([
            { "row_no": 1, "drawing_no": "PO-SOFT" },
            { "row_no": 2, "drawing_no": "PO-SOFT-ASM" },
            { "row_no": 3, "drawing_no": "C-SOFT" }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let data = &env["data"];
    for i in [0usize, 1usize] {
        let r = row(data, i);
        assert_eq!(r["match_type"], "NONE", "第 {i} 行不应命中软删行: {r}");
        assert!(r["parts"].as_array().is_some_and(|p| p.is_empty()));
    }
    // 子件本身未被软删 ⇒ 仍按自己的图号命中（闸门不级联）
    let r = row(data, 2);
    assert_eq!(r["match_type"], "PART_CODE", "{r}");
    assert_eq!(
        part_at(data, 2, 0)["part_id"].as_str(),
        Some(child.to_string().as_str())
    );
}

// ===========================================================================
//  match 端点：数组长度守恒 + row_no 逐个对应
// ===========================================================================

/// 场景 7：3 行请求（图号命中 / 装配件命中 / 未命中）⇒ 长度恰 3，row_no 逐个对应。
#[tokio::test]
async fn match_result_length_and_row_no_align_with_items() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-KEEP-1",
        "命中一",
        None,
        None,
        None,
    )
    .await;
    let asm = insert_assembly(&pool, fx.customer_l2_id, "PO-KEEP-2", "命中装配件").await;
    insert_part(
        &pool,
        fx.customer_l2_id,
        "C-KEEP",
        "子件",
        Some(asm),
        None,
        None,
    )
    .await;

    let (s, env) = call_match(
        app,
        &token,
        json!([
            { "row_no": 100, "line_no": "1", "drawing_no": "PO-KEEP-1" },
            { "row_no": 205, "line_no": "2", "drawing_no": "PO-KEEP-2" },
            { "row_no": 333, "line_no": "3", "drawing_no": "PO-KEEP-NONE" }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let data = env["data"].as_array().unwrap();
    assert_eq!(data.len(), 3, "长度必须恒等于 items 长度: {env}");
    assert_eq!(
        data.iter()
            .map(|r| r["row_no"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![100, 205, 333],
        "row_no 顺序必须与请求一致: {env}"
    );
    assert_eq!(data[0]["match_type"], "PART_CODE", "{env}");
    assert_eq!(data[1]["match_type"], "ASSEMBLY_CODE", "{env}");
    assert_eq!(data[2]["match_type"], "NONE", "{env}");
}

// ===========================================================================
//  match 端点：权限与入参闸门
// ===========================================================================

/// 场景 8：INSPECTOR 调 match ⇒ HTTP 403 + 40300。
#[tokio::test]
async fn match_rejects_inspector() {
    let (_pool, app, token, _fx) = bootstrap_as_inspector().await;
    let (s, env) = call_match(
        app,
        &token,
        json!([{ "row_no": 1, "drawing_no": "PO-001" }]),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "INSPECTOR 应 403: {env}");
    assert_eq!(env["code"], code::FORBIDDEN, "{env}");
}

/// 场景 9：`items: []` ⇒ HTTP 422 + 40001。
#[tokio::test]
async fn match_rejects_empty_items() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = call_match(app, &token, json!([])).await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "空 items 应 422: {env}"
    );
    assert_eq!(env["code"], code::VALIDATION_ERROR, "{env}");
}

/// 场景 9 补充：`items.len() > 2000` ⇒ HTTP 422 + 40001（防一次请求打爆连接池）。
#[tokio::test]
async fn match_rejects_items_over_limit() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let items: Vec<Value> = (0..=2000)
        .map(|i| json!({ "row_no": i, "drawing_no": format!("PO-{i}") }))
        .collect();
    let (s, env) = call_match(app, &token, json!(items)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "超限应 422: {env}");
    assert_eq!(env["code"], code::VALIDATION_ERROR, "{env}");
}

/// 场景 10：前端仍在发**顶层** `doc_no` + **item 级** `delivery_date` /
/// `unit_price` / `quantity`，后端都不声明 ⇒ 必须照常 200。
///
/// `delivery_date` 是**非日期文本**（前端 `parseDateOrNull` 对无法识别的日期原样
/// 透传）。这条用例锁住「后端不许把它声明成 `Option<NaiveDate>`」——一改就会让
/// 这类请求整个 400。
///
/// 2026-10-06 review 第 1 轮 MINOR-6：顶层 `doc_no` 由 `call_match` helper 统一
/// 带上（前端 `PartBatchOrderInfoMatchRequest` 的顶层确实有这个字段，而后端
/// `MatchByExcelItemsRequest` 只有 `items`），此前注释声称覆盖但实际没发。
#[tokio::test]
async fn match_ignores_unknown_item_fields() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-EXTRA",
        "带多余字段",
        None,
        None,
        None,
    )
    .await;

    let (s, env) = call_match(
        app,
        &token,
        json!([{
            "row_no": 1,
            "line_no": "1",
            "drawing_no": "PO-EXTRA",
            "delivery_date": "不是日期",
            "unit_price": 12.5,
            "quantity": 3
        }]),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "顶层 doc_no + item 级未知字段都应被忽略: {env}"
    );
    assert_eq!(
        part_at(&env["data"], 0, 0)["part_id"].as_str(),
        Some(pid.to_string().as_str())
    );
}

// ===========================================================================
//  write 端点：正常路径 + 回读 DB
// ===========================================================================

/// 场景 11：2 件成功 ⇒ `updated_count = 2` / `failed` 空 / `skipped_count = 0`，
/// 且**回读 DB** 确认 `order_no` / `system_delivery_date` 真落库、`version` 真 +1。
#[tokio::test]
async fn update_two_parts_writes_and_bumps_version() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let p1 = insert_part(&pool, fx.customer_l2_id, "PO-U1", "件一", None, None, None).await;
    let p2 = insert_part(&pool, fx.customer_l2_id, "PO-U2", "件二", None, None, None).await;
    // 件二先写上旧值，才能验证「新值真的覆盖了旧值」而不是「原本就空」
    sqlx::query(
        "UPDATE t_part SET order_no = 'OLD', system_delivery_date = '2020-01-01' WHERE id = $1",
    )
    .bind(p2)
    .execute(&pool)
    .await
    .expect("seed old values");

    let (s, env) = call_update(
        app,
        &token,
        json!([
            { "part_id": p1.to_string(), "version": 0, "order_no": "PO-A",
              "system_delivery_date": "2026-10-15" },
            { "part_id": p2.to_string(), "version": 0, "order_no": "PO-B",
              "system_delivery_date": "2026-10-16" }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["code"], 0, "{env}");
    assert_eq!(env["data"]["updated_count"], 2, "{env}");
    assert_eq!(env["data"]["skipped_count"], 0, "{env}");
    assert!(
        env["data"]["failed"]
            .as_array()
            .is_some_and(|f| f.is_empty()),
        "不该有失败件: {env}"
    );

    let (v1, o1, d1) = read_order_info(&pool, p1).await;
    assert_eq!(
        (v1, o1.as_deref(), d1),
        (1, Some("PO-A"), Some(naive_date(2026, 10, 15)))
    );
    let (v2, o2, d2) = read_order_info(&pool, p2).await;
    assert_eq!(
        (v2, o2.as_deref(), d2),
        (1, Some("PO-B"), Some(naive_date(2026, 10, 16)))
    );
}

/// 小工具：`chrono::NaiveDate` 字面量（写断言时可读性更好）。
fn naive_date(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(y, m, d).expect("合法日期")
}

// ===========================================================================
//  write 端点：OCC 冲突 → failed + HTTP 仍 200 + DB 未变
// ===========================================================================

/// 场景 12：`version` 故意给错 ⇒ `updated_count = 0`、每条进 `failed`、
/// `code = 40901`，**HTTP 仍是 200**（前端依赖部分成功语义，不靠 HTTP 码区分）。
#[tokio::test]
async fn update_version_conflict_lands_in_failed_with_http_200() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-CONF",
        "冲突件",
        None,
        Some("KEEP"),
        None,
    )
    .await;

    let (s, env) = call_update(
        app,
        &token,
        json!([{ "part_id": pid.to_string(), "version": 99, "order_no": "SHOULD-NOT-WRITE" }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "OCC 冲突仍是 200 + 信封: {env}");
    assert_eq!(env["code"], 0, "{env}");
    assert_eq!(env["data"]["updated_count"], 0, "{env}");
    assert_eq!(env["data"]["skipped_count"], 0, "{env}");
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{env}");
    assert_eq!(
        failed[0]["part_id"].as_str(),
        Some(pid.to_string().as_str())
    );
    assert_eq!(failed[0]["code"], code::VERSION_CONFLICT, "{env}");
    assert_failure_message_clean(&failed[0], "OCC 冲突");

    // DB 必须原封不动
    let (v, o, d) = read_order_info(&pool, pid).await;
    assert_eq!(
        (v, o.as_deref(), d),
        (0, Some("KEEP"), None),
        "冲突行不得被改"
    );
}

// ===========================================================================
//  write 端点：部分成功（不会一错全错）
// ===========================================================================

/// 场景 13：3 条里 1 条版本错 ⇒ `updated_count = 2` / `failed.len() = 1`，
/// 且两条成功的行**真的**写了库。
#[tokio::test]
async fn update_partial_success_isolates_bad_row() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let ok1 = insert_part(&pool, fx.customer_l2_id, "PO-PS1", "好一", None, None, None).await;
    let bad = insert_part(&pool, fx.customer_l2_id, "PO-PS2", "坏", None, None, None).await;
    let ok2 = insert_part(&pool, fx.customer_l2_id, "PO-PS3", "好二", None, None, None).await;

    let (s, env) = call_update(
        app,
        &token,
        json!([
            { "part_id": ok1.to_string(), "version": 0, "order_no": "OK-1" },
            { "part_id": bad.to_string(), "version": 99, "order_no": "BAD" },
            { "part_id": ok2.to_string(), "version": 0, "order_no": "OK-2" }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["updated_count"], 2, "{env}");
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{env}");
    assert_eq!(
        failed[0]["part_id"].as_str(),
        Some(bad.to_string().as_str())
    );

    let (_, o1, _) = read_order_info(&pool, ok1).await;
    assert_eq!(o1.as_deref(), Some("OK-1"), "坏行不得牵连前后两条");
    let (_, o2, _) = read_order_info(&pool, ok2).await;
    assert_eq!(o2.as_deref(), Some("OK-2"));
    let (_, ob, _) = read_order_info(&pool, bad).await;
    assert_eq!(ob, None, "坏行不得被写");
}

// ===========================================================================
//  write 端点：三态（显式 null 清空 / 缺省不动）
// ===========================================================================

/// 场景 14(a)：传 `system_delivery_date: null` / `order_no: null` ⇒ 两列**真的**
/// 变 NULL。
///
/// 动机：前端 date-picker 可清空，旧语义（单层 `Option`，`None` 一律「不改列」）
/// 下用户清空系统交期是**静默无效**的 —— 接口 200、界面输入框清空、库里原值不动。
#[tokio::test]
async fn update_tristate_clears_column_on_explicit_null() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-CLEAR",
        "待清空",
        None,
        Some("PO-OLD"),
        Some(naive_date(2026, 12, 1)),
    )
    .await;

    let (s, env) = call_update(
        app,
        &token,
        json!([{
            "part_id": pid.to_string(),
            "version": 0,
            "order_no": null,
            "system_delivery_date": null
        }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["updated_count"], 1, "{env}");
    let (v, o, d) = read_order_info(&pool, pid).await;
    assert_eq!(v, 1, "version 仍应自增（列被清空也是一次更新）: {env}");
    assert_eq!(o, None, "order_no 应真的变 NULL: {env}");
    assert_eq!(d, None, "system_delivery_date 应真的变 NULL: {env}");
}

/// 场景 14(b)：字段**缺省** ⇒ 该列不动（与「显式 null」是两个不同的三态）。
#[tokio::test]
async fn update_tristate_absent_field_leaves_column_untouched() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-KEEP",
        "缺省不动",
        None,
        Some("PO-KEEP"),
        Some(naive_date(2026, 12, 2)),
    )
    .await;
    // 先给 note 一个哨兵值，用来证明「没传的字段不会被写成 NULL」
    sqlx::query("UPDATE t_part SET note = 'SENTINEL' WHERE id = $1")
        .bind(pid)
        .execute(&pool)
        .await
        .expect("seed note");

    let (s, env) = call_update(
        app,
        &token,
        json!([{ "part_id": pid.to_string(), "version": 0, "order_no": "PO-NEW" }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["updated_count"], 1, "{env}");
    let (_, o, d) = read_order_info(&pool, pid).await;
    assert_eq!(o.as_deref(), Some("PO-NEW"), "给了值的列应写入: {env}");
    assert_eq!(
        d,
        Some(naive_date(2026, 12, 2)),
        "未传的 system_delivery_date 必须保持原值: {env}"
    );
    let note: Option<String> = sqlx::query_scalar("SELECT note FROM t_part WHERE id = $1")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .expect("read note");
    assert_eq!(
        note.as_deref(),
        Some("SENTINEL"),
        "未传的 note 必须保持原值（缺省 ≠ 写 NULL）: {env}"
    );
}

// ===========================================================================
//  write 端点：skip（人工判定不该回填的行）
// ===========================================================================

/// 场景 15：`skip: true` ⇒ 不写库、计入 `skipped_count`、既不进 updated 也不进 failed。
#[tokio::test]
async fn update_skip_row_does_not_touch_db() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let skip_me = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-SKIP",
        "跳过件",
        None,
        Some("OLD"),
        None,
    )
    .await;
    let write_me = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-WRITE",
        "写入件",
        None,
        Some("OLD"),
        None,
    )
    .await;

    let too_long = "长".repeat(40); // varchar(30)
    let (s, env) = call_update(
        app,
        &token,
        json!([
            // 2026-10-06 review 第 3 轮 R3-8：这一行同时踩**三道**不该触发的闸门 ——
            // version 故意写错（若先做 OCC 再判 skip 会 40901）、系统交期是非日期
            // 文本「待定」、`order_no` 超 varchar(30)。skip 的语义是「这行别碰」，
            // 所以三道闸门对它一律不生效：`failed` 必须仍为空，只计 skipped_count。
            { "part_id": skip_me.to_string(), "version": 99, "skip": true,
              "order_no": too_long, "system_delivery_date": "待定" },
            { "part_id": write_me.to_string(), "version": 0, "order_no": "WRITTEN" }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["updated_count"], 1, "{env}");
    assert_eq!(env["data"]["skipped_count"], 1, "{env}");
    assert!(
        env["data"]["failed"]
            .as_array()
            .is_some_and(|f| f.is_empty()),
        "skip 行不该进 failed: {env}"
    );
    let (v_skip, o_skip, _) = read_order_info(&pool, skip_me).await;
    assert_eq!(
        (v_skip, o_skip.as_deref()),
        (0, Some("OLD")),
        "skip 行 DB 未被改"
    );
    let (_, o_write, _) = read_order_info(&pool, write_me).await;
    assert_eq!(o_write.as_deref(), Some("WRITTEN"), "非 skip 行照常写");
}

// ===========================================================================
//  write 端点：failed[].message 不泄 sqlx 内部文本
// ===========================================================================

/// DB 错误行的 `failed[].message` 不得含 sqlx 原始文本（细节走 `tracing::warn!`
/// 收口，响应体只给统一中文文案）。
///
/// 2026-10-06 review 第 1 轮：旧实现 `format!("{e}")` 把 sqlx 错误原文塞进 200
/// 响应体（表名列名 / 约束名 / 错误码全泄）。
///
/// 2026-10-06 review 第 3 轮 R3-1 换触发手法：本用例原本靠「`order_no` 超
/// `varchar(30)` ⇒ PG 22001」触发 DB 错误，而 R3-1 给 `order_no` 加了**行级长度
/// 闸门**（40001），那个输入在进 SQL 之前就被拦下了，触发点失效。
/// 现改用 **NUL 字符**（`"a\u0000b"`）：PG 对含 NUL 的文本报 `22021
/// invalid byte sequence`，而长度闸门（1 个 char）放行 ⇒ 确实落到 DB 错误分支。
/// 这同时说明闸门没把 DB 错误路径整个堵死。
///
/// ⚠️ 另一种可用手法是「非法日期字符串」，但那条路走不到 SQL：`system_delivery_date`
/// 的 `NaiveDate` 在**反序列化阶段**就拒掉整请求（axum extractor rejection，纯文本
/// body、不是信封）。
#[tokio::test]
async fn update_failure_message_has_no_sqlx_internals() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let good = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-DBERR",
        "好件",
        None,
        None,
        None,
    )
    .await;
    let bad = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-DBERR2",
        "坏件",
        None,
        None,
        None,
    )
    .await;
    let nul = "a\u{0}b";

    let (s, env) = call_update(
        app,
        &token,
        json!([
            { "part_id": good.to_string(), "version": 0, "order_no": "OK" },
            { "part_id": bad.to_string(), "version": 0, "order_no": nul }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "DB 错误也必须是 200 + 信封: {env}");
    assert_eq!(env["code"], 0, "{env}");
    assert_eq!(env["data"]["updated_count"], 1, "好行照常写: {env}");
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{env}");
    assert_eq!(
        failed[0]["part_id"].as_str(),
        Some(bad.to_string().as_str()),
        "{env}"
    );
    assert_eq!(
        failed[0]["code"],
        code::DATABASE,
        "应报 50001 DATABASE: {env}"
    );
    assert_failure_message_clean(&failed[0], "DB 错误行");
    // 失败行确实没落库
    let (_, o, _) = read_order_info(&pool, bad).await;
    assert_eq!(o, None, "DB 报错那行不得被写: {env}");
}

// ===========================================================================
//  write 端点：权限与入参闸门
// ===========================================================================

/// 场景 17(a)：INSPECTOR 调 batch-update ⇒ HTTP 403 + 40300。
#[tokio::test]
async fn update_rejects_inspector() {
    let (_pool, app, token, _fx) = bootstrap_as_inspector().await;
    let (s, env) = call_update(
        app,
        &token,
        json!([{ "part_id": "1", "version": 0, "skip": true }]),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "INSPECTOR 应 403: {env}");
    assert_eq!(env["code"], code::FORBIDDEN, "{env}");
}

/// 场景 17(b)：`items: []` ⇒ HTTP 422 + 40001。
#[tokio::test]
async fn update_rejects_empty_items() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = call_update(app, &token, json!([])).await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "空 items 应 422: {env}"
    );
    assert_eq!(env["code"], code::VALIDATION_ERROR, "{env}");
}

// ===========================================================================
//  write 端点：review 第 1 轮 —— 非法日期文本降级为「该行进 failed」
// ===========================================================================

/// review 第 1 轮 MAJOR-1：`system_delivery_date` 给**非日期文本** ⇒ 该行进
/// `failed[]`（40001）+ **HTTP 仍 200**，其余行照常写库。
///
/// 改动前 DTO 是 `Option<Option<NaiveDate>>`，这类文本在 **axum JsonRejection** 层
/// 就 400 且 body 是**纯文本、不是 `R` 信封** ⇒ 用户已勾选的整批回填全部作废、
/// 前端连错误码都读不到。真实来源：前端 `parseDateOrNull`
/// （`purchaseOrderExcelParser.ts:116` 对 dayjs 认不出的文本 `return text` 原样透传）
/// + `el-date-picker` 不洗 model 值（`use-common-picker.mjs:25-40`）。
#[tokio::test]
async fn update_invalid_date_text_fails_only_that_row() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let ok1 = insert_part(&pool, fx.customer_l2_id, "PO-DT1", "好一", None, None, None).await;
    let bad = insert_part(&pool, fx.customer_l2_id, "PO-DT2", "坏一", None, None, None).await;
    let bad2 = insert_part(&pool, fx.customer_l2_id, "PO-DT3", "坏二", None, None, None).await;
    let ok2 = insert_part(&pool, fx.customer_l2_id, "PO-DT4", "好二", None, None, None).await;

    let (s, env) = call_update(
        app,
        &token,
        json!([
            { "part_id": ok1.to_string(), "version": 0, "order_no": "OK-1",
              "system_delivery_date": "2026-10-15" },
            { "part_id": bad.to_string(), "version": 0, "order_no": "BAD-1",
              "system_delivery_date": "2026年8月1日" },
            { "part_id": bad2.to_string(), "version": 0, "order_no": "BAD-2",
              "system_delivery_date": "待定" },
            { "part_id": ok2.to_string(), "version": 0, "order_no": "OK-2" }
        ]),
    )
    .await;
    // 关键：整个请求不再 400（改动前这里是 JsonRejection 的纯文本 400）
    assert_eq!(s, StatusCode::OK, "非法日期不得打掉整请求: {env}");
    assert_eq!(env["code"], 0, "{env}");
    assert_eq!(
        env["data"]["updated_count"], 2,
        "合法日期 + 缺省两行写成功: {env}"
    );
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 2, "两条非法日期各进 failed: {env}");
    assert_eq!(
        failed[0]["code"],
        code::VALIDATION_ERROR,
        "应报 40001: {env}"
    );
    assert_eq!(
        failed[1]["code"],
        code::VALIDATION_ERROR,
        "应报 40001: {env}"
    );
    assert_eq!(
        failed[0]["part_id"].as_str(),
        Some(bad.to_string().as_str())
    );
    assert_eq!(
        failed[1]["part_id"].as_str(),
        Some(bad2.to_string().as_str())
    );
    let msg = failed[0]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("系统交期") && msg.contains("2026年8月1日"),
        "message 应说明是系统交期问题并回显原值，实际 {msg:?}"
    );
    assert_failure_message_clean(&failed[0], "非法日期行");

    // 坏行不得被写（连 order_no 也不写：解析失败即整行跳过，不做部分写）
    for (bad_id, order_no) in [(bad, "BAD-1"), (bad2, "BAD-2")] {
        let (v, o, d) = read_order_info(&pool, bad_id).await;
        assert_eq!((v, o, d), (0, None, None), "{order_no} 行不得被写: {env}");
    }
    // 好行照常写：合法日期落库；缺省日期那一列不动
    let (v1, o1, d1) = read_order_info(&pool, ok1).await;
    assert_eq!(
        (v1, o1.as_deref(), d1),
        (1, Some("OK-1"), Some(naive_date(2026, 10, 15)))
    );
    let (v2, o2, d2) = read_order_info(&pool, ok2).await;
    assert_eq!(
        (v2, o2.as_deref(), d2),
        (1, Some("OK-2"), None),
        "缺省系统交期那一行: {env}"
    );
}

/// review 第 1 轮 MAJOR-1 的**向后兼容**一侧：合法日期文本的行为与改动前**完全
/// 一致** —— 照常解析、写库、version +1。
///
/// 「两端空白」这个输入是刻意加的（2026-10-06 review 第 3 轮 R3-2 订正上一轮的
/// 因果误述）：chrono's `FromStr`（= 改动前 `NaiveDate` 的 serde 反序列化内部走的
/// 同一条路径）在 `Item::Space("")` 处**跳任意量空白**，所以 `" 2026-10-15 "` 在
/// **改动前就是被接受的**。本轮改成 `String` + service 侧解析后刻意继续走同一个
/// `FromStr`，trim 只是让「展示用原文」与「解析用串」一致 —— 它**不是**新增的放宽，
/// 也不是「原先会 400」。（已用 chrono 0.4.45 逐值实测：`FromStr(" 2026-07-08 ")`
/// = `Ok`，而 `parse_from_str(" 2026-07-08 ", "%Y-%m-%d")` = `Err(TooLong)`。）
/// 本用例的作用是**守住这个等价性**，别让后人把它换成 `parse_from_str`。
#[tokio::test]
async fn update_valid_date_with_surrounding_space_still_writes() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-DT-OK",
        "合法",
        None,
        None,
        None,
    )
    .await;

    let (s, env) = call_update(
        app,
        &token,
        json!([{ "part_id": pid.to_string(), "version": 0,
                 "system_delivery_date": " 2026-10-15 " }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["updated_count"], 1, "{env}");
    assert!(
        env["data"]["failed"]
            .as_array()
            .is_some_and(|f| f.is_empty()),
        "合法日期不得进 failed: {env}"
    );
    let (v, _, d) = read_order_info(&pool, pid).await;
    assert_eq!(v, 1, "{env}");
    assert_eq!(
        d,
        Some(naive_date(2026, 10, 15)),
        "带空白的合法日期应照常落库: {env}"
    );
}

// ===========================================================================
//  write 端点：review 第 1 轮 —— 软删闸门 / items 上限
// ===========================================================================

/// review 第 1 轮 MINOR-4：软删的 part 不会被回填。
///
/// `update_order_info` 的 WHERE 带 `deleted_at IS NULL`，但此前**零测试**证明它。
/// 契约明写「OCC / 软删 / 不存在 → 40901」，且软删闸门在所有写路径生效是安全项
/// （软删件不该再被业务数据「复活」）。
#[tokio::test]
async fn update_soft_deleted_part_is_not_backfilled() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-SOFT-W",
        "软删件",
        None,
        Some("KEEP"),
        None,
    )
    .await;
    soft_delete_part(&pool, pid).await;
    let deleted_at_before: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM t_part WHERE id = $1")
            .bind(pid)
            .fetch_one(&pool)
            .await
            .expect("read deleted_at");

    let (s, env) = call_update(
        app,
        &token,
        json!([{ "part_id": pid.to_string(), "version": 1, "order_no": "SHOULD-NOT-WRITE" }]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["updated_count"], 0, "软删件不得被回填: {env}");
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{env}");
    assert_eq!(failed[0]["code"], code::VERSION_CONFLICT, "{env}");
    assert_failure_message_clean(&failed[0], "软删件");
    let (v, o, _) = read_order_info(&pool, pid).await;
    assert_eq!(o.as_deref(), Some("KEEP"), "软删件的行不得被改: {env}");
    assert_eq!(v, 1, "version 也不得因失败请求而变: {env}");
    let deleted_at_after: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM t_part WHERE id = $1")
            .bind(pid)
            .fetch_one(&pool)
            .await
            .expect("read deleted_at");
    assert_eq!(
        deleted_at_after, deleted_at_before,
        "deleted_at 不得被改: {env}"
    );
}

/// review 第 1 轮 MINOR-5：`items.len() > 2000` ⇒ HTTP 422 + 40001，且**不入循环**
/// （一条 UPDATE 都不该发 —— 写端点每行一次往返，N 万行会把那条池化连接占死）。
#[tokio::test]
async fn update_rejects_items_over_limit() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-LIMIT",
        "限额件",
        None,
        None,
        None,
    )
    .await;

    let items: Vec<Value> = (0..=2000)
        .map(|_| json!({ "part_id": pid.to_string(), "version": 0, "order_no": "NOPE" }))
        .collect();
    let (s, env) = call_update(app, &token, json!(items)).await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "超 2000 行应 422: {env}"
    );
    assert_eq!(env["code"], code::VALIDATION_ERROR, "{env}");
    // 整单拒 ⇒ 入参里的 2001 行一条都不能被写（超限检查在循环之前）
    let (v, o, _) = read_order_info(&pool, pid).await;
    assert_eq!((v, o), (0, None), "超限时不得有任何一行写库: {env}");
}

// ===========================================================================
//  write 端点：review 第 3 轮 —— order_no / note 长度闸门
// ===========================================================================

/// review 第 3 轮 R3-1：`order_no` 超 `varchar(30)` ⇒ **行级 40001**、该行 DB
/// 完全未动，其余行照常写。
///
/// 上一轮只给 `system_delivery_date` 做了行级闸门，`order_no` 仍走「打 DB ⇒ 50001」，
/// 两个兄弟列两种语义；而 50001 会把排障方向误导到数据库。
/// 触发路径在本功能里**不需要用户犯蠢**：前端
/// `purchaseOrderExcelParser.ts:155` 把采购订单「单据编号」首行原文（无长度校验、
/// 无截断）读进 `docNo`，`PurchaseOrderImportDialog.vue:636` 把它作为**每一个**
/// 候选行的默认 `orderNo` ⇒ 一张 PO 的单据编号超 30 字，整批每一行都撞库。
#[tokio::test]
async fn update_order_no_too_long_fails_only_that_row() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let ok1 = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-LEN1",
        "好一",
        None,
        None,
        None,
    )
    .await;
    let bad = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-LEN2",
        "超长",
        None,
        Some("OLD"),
        None,
    )
    .await;
    let ok2 = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-LEN3",
        "好二",
        None,
        None,
        None,
    )
    .await;
    let too_long = "长".repeat(40); // varchar(30)

    let (s, env) = call_update(
        app,
        &token,
        json!([
            { "part_id": ok1.to_string(), "version": 0, "order_no": "OK-1" },
            { "part_id": bad.to_string(), "version": 0, "order_no": too_long },
            { "part_id": ok2.to_string(), "version": 0, "order_no": "OK-2" }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "超长不该打掉整批: {env}");
    assert_eq!(env["data"]["updated_count"], 2, "另两行照常写: {env}");
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{env}");
    assert_eq!(
        failed[0]["code"],
        code::VALIDATION_ERROR,
        "应报 40001 而非 50001: {env}"
    );
    assert_eq!(
        failed[0]["part_id"].as_str(),
        Some(bad.to_string().as_str())
    );
    let msg = failed[0]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("订单号") && msg.contains("30"),
        "message 应说明是订单号超长并给出上限，实际 {msg:?}"
    );
    assert_failure_message_clean(&failed[0], "订单号超长行");

    // 该行 DB 完全未动：order_no 保持原值、version 不变（连 OCC 检查都没走到）
    let (v, o, _) = read_order_info(&pool, bad).await;
    assert_eq!(
        (v, o.as_deref()),
        (0, Some("OLD")),
        "超长行 DB 不得被改: {env}"
    );
    let (_, o1, _) = read_order_info(&pool, ok1).await;
    assert_eq!(o1.as_deref(), Some("OK-1"));
    let (_, o2, _) = read_order_info(&pool, ok2).await;
    assert_eq!(o2.as_deref(), Some("OK-2"));
}

/// review 第 3 轮 R3-1：`note` 超 `varchar(500)` ⇒ 同样行级 40001。
///
/// 单独一条用例：`note` 是三态里最容易被忽略的一列（前端在导入对话框里并不暴露
/// note 字段），闸门与 `order_no` 同构但列不同，值得各自锁一次。
#[tokio::test]
async fn update_note_too_long_fails_only_that_row() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let bad = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-NOTE",
        "备注超长",
        None,
        None,
        None,
    )
    .await;
    let ok = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-NOTE2",
        "正常",
        None,
        None,
        None,
    )
    .await;
    let too_long = "备".repeat(600); // varchar(500)

    let (s, env) = call_update(
        app,
        &token,
        json!([
            { "part_id": bad.to_string(), "version": 0, "note": too_long },
            { "part_id": ok.to_string(), "version": 0, "note": "短备注" }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["updated_count"], 1, "{env}");
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{env}");
    assert_eq!(failed[0]["code"], code::VALIDATION_ERROR, "{env}");
    let msg = failed[0]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("备注") && msg.contains("500"),
        "message 应说明是备注超长并给出上限，实际 {msg:?}"
    );
    let note: Option<String> = sqlx::query_scalar("SELECT note FROM t_part WHERE id = $1")
        .bind(bad)
        .fetch_one(&pool)
        .await
        .expect("read note");
    assert_eq!(note, None, "超长行 note 不得被写入: {env}");
    let note_ok: Option<String> = sqlx::query_scalar("SELECT note FROM t_part WHERE id = $1")
        .bind(ok)
        .fetch_one(&pool)
        .await
        .expect("read note");
    assert_eq!(note_ok.as_deref(), Some("短备注"), "正常行照常写: {env}");
}

/// review 第 3 轮 R3-1 的口径补充：长度按 **char** 计数（不是字节），按**原长度**判
/// （不 trim）。
///
/// 这条用例把三个容易写错的边界钉住：
/// - 30 个**中文字符**（90 字节）必须放行 —— 按字节判会误杀真实的中文订单号；
/// - 31 字被拒，且报 **40001** 而不是漏成 50001；
/// - **两端带空白**的 30 字串（共 36 字）也要被拒：闸门量的是「将要写进
///   `varchar(30)` 的那串字符」的原长度。若实现 trim 后计数却把原串写库，就会
///   出现「过闸门 → PG 拒 → 50001」这个洞，正是本闸门要消灭的排障歧路。
#[tokio::test]
async fn update_length_gate_counts_chars_not_bytes() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let cn30 = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-CN30",
        "中文 30 字",
        None,
        None,
        None,
    )
    .await;
    let cn31 = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-CN31",
        "中文 31 字",
        None,
        None,
        None,
    )
    .await;
    let padded = insert_part(
        &pool,
        fx.customer_l2_id,
        "PO-PAD",
        "带空白",
        None,
        None,
        None,
    )
    .await;
    let cn30_text = "订".repeat(30); // 30 char / 90 byte
    let cn31_text = "订".repeat(31);
    let padded_text = format!("   {}   ", "订".repeat(30)); // 36 char

    let (s, env) = call_update(
        app,
        &token,
        json!([
            { "part_id": cn30.to_string(), "version": 0, "order_no": cn30_text },
            { "part_id": cn31.to_string(), "version": 0, "order_no": cn31_text },
            { "part_id": padded.to_string(), "version": 0, "order_no": padded_text }
        ]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["updated_count"], 1,
        "只有 30 字中文（90 字节）那行放行: {env}"
    );
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 2, "31 字与带空白的 36 字都应被拒: {env}");
    for f in failed {
        assert_eq!(
            f["code"],
            code::VALIDATION_ERROR,
            "长度闸门漏成 50001 就等于没修（trim 后计数 / 原串写库的洞）: {f}"
        );
    }
    let (_, o30, _) = read_order_info(&pool, cn30).await;
    assert_eq!(
        o30.as_deref(),
        Some(cn30_text.as_str()),
        "30 字中文（90 字节）应落库 —— 按字节判会误杀真实订单号: {env}"
    );
    let (_, o31, _) = read_order_info(&pool, cn31).await;
    assert_eq!(o31, None, "31 字不得被写: {env}");
    let (_, opad, _) = read_order_info(&pool, padded).await;
    assert_eq!(opad, None, "带空白的 36 字不得被写: {env}");
}
