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
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::*;

// ===========================================================================
//  本文件私有 helper（与 crud.rs / create_serial_price.rs 同风格：sub-file 私有）
// ===========================================================================

/// 本文件私有的雪花 ID 生成器。
///
/// 2026-10-06：不复用 `test_support::pool_snowflake()`（它被 `tests/part/crud.rs`
/// 的 `shared_test_snowflake` 各自的域内单例占着语义），单独一个 instance 段，
/// 避免与其它 sub-file 在**同一测试库**里撞 id（本文件一个用例常插 20+ 行）。
fn po_snowflake() -> &'static SnowflakeIdGenerator {
    static GEN: std::sync::OnceLock<SnowflakeIdGenerator> = std::sync::OnceLock::new();
    GEN.get_or_init(|| SnowflakeIdGenerator::new(1_577_836_800_000, 777))
}

fn next_id() -> i64 {
    po_snowflake().next_id()
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
async fn call_match(app: axum::Router, token: &str, items: Value) -> (StatusCode, Value) {
    send(
        app,
        json_request(
            "POST",
            "/parts/match-by-excel-items",
            Some(json!({ "items": items })),
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
//  1. PART_CODE 命中 + 现值逐字段回显
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
//  2. 同图号多候选（cap 以内全给，顺序可断言）
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
//  3. 名称兜底 PART_NAME
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
//  4. 装配件图号 ASSEMBLY_CODE：候选是子件 + assembly_name
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
//  5. 未匹配行仍出现且 parts 是空数组
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
//  6. 软删不参与
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
//  7. 数组长度守恒 + row_no 逐个对应
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
//  8/9. 权限与入参闸门
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

/// 场景 10：前端仍在发 `doc_no` / `delivery_date` / `unit_price` / `quantity`，
/// 后端不声明它们 ⇒ 必须照常 200。
///
/// `delivery_date` 是**非日期文本**（前端 `parseDateOrNull` 对无法识别的日期原样
/// 透传）。这条用例锁住「后端不许把它声明成 `Option<NaiveDate>`」——一改就会让
/// 这类请求整个 400。
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
    assert_eq!(s, StatusCode::OK, "未知字段应被 serde 忽略: {env}");
    assert_eq!(
        part_at(&env["data"], 0, 0)["part_id"].as_str(),
        Some(pid.to_string().as_str())
    );
}

// ===========================================================================
//  11. update 正常路径 + 回读 DB
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
//  12. OCC 冲突 → failed + HTTP 仍 200 + DB 未变
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
//  13. 部分成功：不会一错全错
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
//  14. 三态：显式 null 清空
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
//  15. skip：人工判定不该回填的行
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

    let (s, env) = call_update(
        app,
        &token,
        json!([
            // version 故意写错 + skip=true：若实现先做 OCC 再判 skip，本例会 40901
            { "part_id": skip_me.to_string(), "version": 99, "order_no": "NOPE", "skip": true },
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
//  16. failed[].message 不泄 sqlx 内部文本（DB 错误路径）
// ===========================================================================

/// 场景 16：让某一行触发**真实 DB 错误** ⇒ 该行进 `failed` 且 `code = 50001`，
/// 而 `message` **不得**含 sqlx / 表名 / 列名等 internals。
///
/// 触发手法：`t_part.order_no` 是 `varchar(30)`，入参给 40 字 → PG 报 22001
/// value too long ⇒ sqlx `Database(22001)`。service 侧把该错误收进 `failed`
/// （不抛、其余行照写），细节走 `tracing::warn!`，响应体只给统一中文文案。
///
/// 2026-10-06：旧实现 `format!("{e}")` 把 sqlx 错误原文塞进 200 响应体。
///
/// ⚠️ 不能用「非法日期字符串」触发本场景：`system_delivery_date` 的类型是
/// `NaiveDate`，chrono 在**反序列化阶段**就拒掉整请求（axum  extractor rejection，
/// 纯文本 body、不是信封），根本走不到 SQL。用长度超限的 `order_no` 才能落到 DB。
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
    let too_long = "长".repeat(40); // varchar(30) ⇒ PG 22001

    let (s, env) = call_update(
        app,
        &token,
        json!([
            { "part_id": good.to_string(), "version": 0, "order_no": "OK" },
            { "part_id": bad.to_string(), "version": 0, "order_no": too_long }
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
//  17. batch-update 的权限与入参闸门
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
