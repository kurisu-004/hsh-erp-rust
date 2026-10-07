//! `GET /api/v2/com/delivery/note/scan/{serial_no}` 扫码三层树端到端测试
//!
//! 树形状与 `prod::inspection` 的扫码树**同形**（同一前端树组件），本域额外需要
//! `draft` / `customer_id` / `entry_max_*` / `occupied_by_note_no` 四个字段。
//!
//! 覆盖
//! 1. 命中口径：先 part 后 assembly；装配件子件条码 → 整棵装配件树；未命中 ⇒ 404 /
//!    20101（**复用 part 域的码**，不是送货单的 21417）
//! 2. `serial_no` 含 `-` 时可路由（路径参数是 `Path<String>` 不是数值提取器）
//! 3. 软删闸门：软删零件 / 软删装配件
//! 4. 批次层**不过滤 status**（终态也在树里）+ `occupied_by_note_no`
//! 5. `entry_max_quantity`（散件）与 `entry_max_sets` / `per_set_parts`（装配件）
//! 6. `draft` 判定：命中既有 DRAFT 就返回、无则 `null`，且**本端点绝不建单**
//! 7. `per_set_quantity` 的整数除法向零截断
//! 8. SQL 条数：批次层一条 `part_id = ANY($1)` 取回整棵树（无 N+1）—— 用「子件 5 个
//!    各 3 批次 = 15 个批次一次返回」钉住（若退化成 per-part 查询，这里仍然绿，
//!    故另用 pg_stat_statements 不可得；本用例锁的是「一次请求全树完整返回」）

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    DeliveryFixture, PartFixture, json_request, load_delivery_fixture, load_part_fixture,
    login_token, send, test_app, test_pool, test_state,
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

/// 调扫码树端点（序列号原样放进 URL —— 前端必须 `encodeURIComponent`）。
async fn scan_tree(app: &axum::Router, token: &str, serial_no: &str) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request(
            "GET",
            &format!("/com/delivery/note/scan/{serial_no}"),
            None,
            Some(token),
        ),
    )
    .await
}

async fn insert_l1(pool: &PgPool, name: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 11).next_id();
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
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 11).next_id();
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

async fn insert_part(
    pool: &PgPool,
    name: &str,
    serial_no: &str,
    customer_id: i64,
    assembly_id: Option<i64>,
    quantity: i32,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 11).next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by, assembly_id) \
         VALUES ($1, $2, $3, 'D-TREE', $4, 'READY_TO_SHIP', '树测试', $5, $5, $6, 0, \
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

async fn insert_assembly(
    pool: &PgPool,
    name: &str,
    serial: &str,
    customer_id: i64,
    quantity: i32,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 11).next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'D-ATREE', $4, 'ACTIVE', '树测试', $5, $5, $6, 0, \
         $7, NULL, $7, NULL)",
    )
    .bind(id)
    .bind(Some(serial.to_string()))
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

async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: &str,
    note_id: Option<i64>,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 11).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         delivery_note_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 'PRODUCTION_SHELF', $6, 0, $7, NULL, $7, NULL)",
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

async fn insert_draft_note(pool: &PgPool, l1_id: i64, no: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 11).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_delivery_note (id, delivery_note_no, customer_id, delivery_date, \
         status, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 'DRAFT', 3, $5, NULL, $5, NULL)",
    )
    .bind(id)
    .bind(no)
    .bind(l1_id)
    .bind(now.date())
    .bind(now)
    .execute(pool)
    .await
    .expect("insert draft note");
    id
}

/// 找 `children[]` 里某零件的节点。
fn part_node(data: &Value, part_id: i64) -> &Value {
    data["children"]
        .as_array()
        .expect("children 必须是数组")
        .iter()
        .find(|p| p["id"].as_str() == Some(part_id.to_string().as_str()))
        .unwrap_or_else(|| panic!("树里找不到 part {part_id}: {data}"))
}

// ===========================================================================
//  1. 命中口径
// ===========================================================================

/// 独立件：命中 `t_part` ⇒ `hit_kind = "PART"`、`assembly = null`、`children = [该件]`。
#[tokio::test]
async fn standalone_part_hit_returns_part_tree() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树独立件").await;
    let l2 = insert_l2(&pool, "树独立件二厂", l1).await;
    let part = insert_part(&pool, "独立件", "T-SOLO", l2, None, 8).await;
    insert_batch(&pool, part, 1, 4, "READY_TO_SHIP", None).await;

    let (s, env) = scan_tree(&app, &token, "T-SOLO").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let d = &env["data"];
    assert_eq!(d["hit_kind"], "PART");
    assert_eq!(d["scanned_serial_no"], "T-SOLO");
    assert!(d["assembly"].is_null(), "独立件树没有装配件节点: {env}");
    assert_eq!(d["children"].as_array().unwrap().len(), 1);
    assert_eq!(d["children"][0]["id"].as_str().unwrap(), part.to_string());
}

/// 装配件条码：命中 `t_assembly` ⇒ `hit_kind = "ASSEMBLY"`、树含该装配件的**全部**
/// 子件（不止被扫中的那个 —— 装配件码没有零件身份，全部批次 `is_scanned = false`）。
#[tokio::test]
async fn assembly_serial_returns_all_children() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树装配件").await;
    let l2 = insert_l2(&pool, "树装配件二厂", l1).await;
    let asm = insert_assembly(&pool, "树装配件体", "T-ASM-1", l2, 5).await;
    let a = insert_part(&pool, "树子件A", "T-ASM-1-01", l2, Some(asm), 10).await;
    let b = insert_part(&pool, "树子件B", "T-ASM-1-02", l2, Some(asm), 5).await;
    insert_batch(&pool, a, 1, 4, "READY_TO_SHIP", None).await;
    insert_batch(&pool, b, 1, 2, "READY_TO_SHIP", None).await;

    let (s, env) = scan_tree(&app, &token, "T-ASM-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let d = &env["data"];
    assert_eq!(d["hit_kind"], "ASSEMBLY");
    assert_eq!(d["assembly"]["id"].as_str().unwrap(), asm.to_string());
    assert_eq!(
        d["children"].as_array().unwrap().len(),
        2,
        "全部子件都在树里"
    );
    for serial in ["T-ASM-1-01", "T-ASM-1-02"] {
        let node = d["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["serial_no"].as_str() == Some(serial))
            .unwrap_or_else(|| panic!("缺子件 {serial}: {env}"));
        assert!(
            node["children"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["is_scanned"] == false),
            "扫装配件码时没有「被扫中的零件」，全部 is_scanned=false: {node}"
        );
    }
}

/// 装配件**子件**条码 ⇒ `hit_kind = "PART"`，但树是**整棵装配件树**（含未扫中的兄弟子件），
/// 且只有被扫中的那个零件的批次 `is_scanned = true`。
#[tokio::test]
async fn assembly_child_serial_returns_whole_assembly_tree_with_highlight() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树子件命中").await;
    let l2 = insert_l2(&pool, "树子件命中二厂", l1).await;
    let asm = insert_assembly(&pool, "子件命中体", "T-ASM-2", l2, 4).await;
    let a = insert_part(&pool, "高亮件", "T-ASM-2-01", l2, Some(asm), 8).await;
    let b = insert_part(&pool, "兄弟件", "T-ASM-2-02", l2, Some(asm), 4).await;
    insert_batch(&pool, a, 1, 3, "READY_TO_SHIP", None).await;
    insert_batch(&pool, b, 1, 2, "READY_TO_SHIP", None).await;

    let (s, env) = scan_tree(&app, &token, "T-ASM-2-01").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let d = &env["data"];
    assert_eq!(d["hit_kind"], "PART", "子件码先命中 t_part");
    assert_eq!(d["assembly"]["id"].as_str().unwrap(), asm.to_string());
    assert_eq!(d["children"].as_array().unwrap().len(), 2);
    assert_eq!(
        part_node(d, a)["children"][0]["is_scanned"],
        true,
        "被扫中的零件批次高亮"
    );
    assert_eq!(
        part_node(d, b)["children"][0]["is_scanned"],
        false,
        "兄弟子件不高亮"
    );
}

/// 序列号含 `-`（装配件子件序列号形态）能正常路由 —— 端点收 `Path<String>` 而不是
/// 数值提取器，前端 `encodeURIComponent` 后即可。
#[tokio::test]
async fn serial_with_dash_routes_as_string_path_param() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树破折号").await;
    let l2 = insert_l2(&pool, "树破折号二厂", l1).await;
    insert_part(&pool, "破折号件", "T-A-B-C-01", l2, None, 3).await;

    let (s, env) = scan_tree(&app, &token, "T-A-B-C-01").await;
    assert_eq!(s, StatusCode::OK, "含 '-' 的序列号应命中: {env}");
    assert_eq!(env["data"]["scanned_serial_no"], "T-A-B-C-01");
}

/// 两表皆未命中 ⇒ 404 / `20101 BIZ_PART_NOT_FOUND`（**复用 part 域的码**，不是送货单
/// 自己的 21417 —— 语义是「扫到的东西不存在」，前端按同一个码弹「未找到」即可）。
#[tokio::test]
async fn unknown_serial_returns_404_20101() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = scan_tree(&app, &token, "T-NOT-EXIST").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20101);
}

/// 空串 / 纯空白按未命中处理（40001 之前就短路，不会打 DB）。
#[tokio::test]
async fn blank_serial_returns_404() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app.clone(),
        json_request("GET", "/com/delivery/note/scan/%20", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "空白序列号按未命中: {env}");
    assert_eq!(env["code"], 20101);
}

// ===========================================================================
//  2. 软删闸门
// ===========================================================================

/// 软删零件扫不到（软删闸门写在 SQL 里，不是内存过滤）。
#[tokio::test]
async fn soft_deleted_part_is_not_hit() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树软删件").await;
    let l2 = insert_l2(&pool, "树软删件二厂", l1).await;
    let part = insert_part(&pool, "软删件", "T-SOFT-1", l2, None, 5).await;
    sqlx::query("UPDATE t_part SET deleted_at = now() WHERE id = $1")
        .bind(part)
        .execute(&pool)
        .await
        .expect("soft delete part");

    let (s, env) = scan_tree(&app, &token, "T-SOFT-1").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "软删件扫不到: {env}");
}

/// 子件的父装配件被软删 ⇒ **退化成独立件树**（`assembly = null` + `children =
/// [被扫中的那个]`），而不是返回一棵「有子件但没有装配件节点」的孤儿树。
#[tokio::test]
async fn soft_deleted_parent_assembly_degrades_to_standalone_tree() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树孤儿").await;
    let l2 = insert_l2(&pool, "树孤儿二厂", l1).await;
    let asm = insert_assembly(&pool, "孤儿体", "T-ORPH-1", l2, 3).await;
    let a = insert_part(&pool, "孤儿件", "T-ORPH-1-01", l2, Some(asm), 6).await;
    sqlx::query("UPDATE t_assembly SET deleted_at = now() WHERE id = $1")
        .bind(asm)
        .execute(&pool)
        .await
        .expect("soft delete assembly");

    let (s, env) = scan_tree(&app, &token, "T-ORPH-1-01").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let d = &env["data"];
    assert!(d["assembly"].is_null(), "父装配件软删 ⇒ 退化: {env}");
    assert_eq!(d["children"].as_array().unwrap().len(), 1);
    assert_eq!(d["children"][0]["id"].as_str().unwrap(), a.to_string());
}

// ===========================================================================
//  3. 批次层不过滤 status + occupied_by_note_no
// ===========================================================================

/// 批次层**不过滤 status**：`COMPLETED` / `CANCELLED` 等终态也在树里（状态闸门在前端）。
///
/// 理由与 `prod::inspection` 同：扫码弹窗要回答「这批货总共分了几批、每批现在什么
/// 状态」，砍掉终态就答不了。
#[tokio::test]
async fn batch_layer_does_not_filter_terminal_statuses() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树终态").await;
    let l2 = insert_l2(&pool, "树终态二厂", l1).await;
    let part = insert_part(&pool, "终态件", "T-TERM-1", l2, None, 9).await;
    insert_batch(&pool, part, 1, 3, "READY_TO_SHIP", None).await;
    insert_batch(&pool, part, 2, 3, "COMPLETED", None).await;
    insert_batch(&pool, part, 3, 3, "CANCELLED", None).await;

    let (s, env) = scan_tree(&app, &token, "T-TERM-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let kids = part_node(&env["data"], part)["children"]
        .as_array()
        .unwrap();
    assert_eq!(kids.len(), 3, "终态批次也必须出现: {env}");
    let statuses: Vec<&str> = kids.iter().map(|b| b["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, vec!["READY_TO_SHIP", "COMPLETED", "CANCELLED"]);
}

/// `occupied_by_note_no`：批次被某张**未软删**送货单占用时给出单号；占用方软删后
/// 视为未占用（`null`）。
#[tokio::test]
async fn occupied_by_note_no_reflects_active_note_and_ignores_soft_deleted() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树占用").await;
    let l2 = insert_l2(&pool, "树占用二厂", l1).await;
    let part = insert_part(&pool, "占用件", "T-OCC-1", l2, None, 6).await;
    let note_id = insert_draft_note(&pool, l1, "DN-OCC-0001").await;
    insert_batch(&pool, part, 1, 3, "READY_TO_SHIP", None).await;
    insert_batch(&pool, part, 2, 3, "READY_TO_SHIP", Some(note_id)).await;

    let (s, env) = scan_tree(&app, &token, "T-OCC-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let kids = part_node(&env["data"], part)["children"]
        .as_array()
        .unwrap();
    assert!(
        kids[0]["occupied_by_note_no"].is_null(),
        "未占用 ⇒ null: {env}"
    );
    assert_eq!(
        kids[1]["occupied_by_note_no"], "DN-OCC-0001",
        "被占用 ⇒ 单号: {env}"
    );

    // 软删占用方后，该批次视为未占用
    sqlx::query("UPDATE t_delivery_note SET deleted_at = now() WHERE id = $1")
        .bind(note_id)
        .execute(&pool)
        .await
        .expect("soft delete note");
    let (s2, env2) = scan_tree(&app, &token, "T-OCC-1").await;
    assert_eq!(s2, StatusCode::OK, "{env2}");
    let kids2 = part_node(&env2["data"], part)["children"]
        .as_array()
        .unwrap();
    assert!(
        kids2[1]["occupied_by_note_no"].is_null(),
        "占用方软删 ⇒ 视为未占用: {env2}"
    );
}

// ===========================================================================
//  4. entry_max_* / per_set_parts
// ===========================================================================

/// `entry_max_quantity`（零件级）= 该零件「可入单」批次（`READY_TO_SHIP` + 未占用 +
/// 未软删）的 `quantity` 合计。终态 / `INSPECTION` / 已占用的批次都不计入。
#[tokio::test]
async fn entry_max_quantity_sums_only_entryable_batches() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树可入单量").await;
    let l2 = insert_l2(&pool, "树可入单量二厂", l1).await;
    let part = insert_part(&pool, "可入单件", "T-EMQ-1", l2, None, 20).await;
    let note_id = insert_draft_note(&pool, l1, "DN-EMQ-0001").await;
    insert_batch(&pool, part, 1, 4, "READY_TO_SHIP", None).await; // 计入 4
    insert_batch(&pool, part, 2, 5, "READY_TO_SHIP", None).await; // 计入 5
    insert_batch(&pool, part, 3, 6, "INSPECTION", None).await; // 不计
    insert_batch(&pool, part, 4, 7, "READY_TO_SHIP", Some(note_id)).await; // 已占用，不计

    let (s, env) = scan_tree(&app, &token, "T-EMQ-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let node = part_node(&env["data"], part);
    assert_eq!(
        node["entry_max_quantity"], 9,
        "只计 READY_TO_SHIP + 未占用: {env}"
    );
    assert_eq!(
        node["children"].as_array().unwrap().len(),
        4,
        "树里仍是 4 个批次"
    );
}

/// `entry_max_sets`（装配件级）与 `per_set_parts`：`entry_max_sets` 复用
/// `shippable_sets` 公式（与详情 VO 的 `shippable_sets` 同源）；`per_set_quantity =
/// part.quantity / assembly.quantity`（**整数除法向零截断**）。
///
/// 数据：装配件 10 套；子件 A 整单 25 件（2.5 → 截断 2）、子件 B 整单 30 件（3）。
/// 可入单：A 5 件、B 6 件 ⇒ per_set 分别为 `5*10/25 = 2` 与 `6*10/30 = 2`
/// ⇒ `entry_max_sets = min(2, 2) = 2`。
#[tokio::test]
async fn entry_max_sets_and_per_set_parts_use_truncating_division() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树套数").await;
    let l2 = insert_l2(&pool, "树套数二厂", l1).await;
    let asm = insert_assembly(&pool, "树套数体", "T-EMS-1", l2, 10).await;
    let a = insert_part(&pool, "套数件A", "T-EMS-1-01", l2, Some(asm), 25).await;
    let b = insert_part(&pool, "套数件B", "T-EMS-1-02", l2, Some(asm), 30).await;
    insert_batch(&pool, a, 1, 5, "READY_TO_SHIP", None).await;
    insert_batch(&pool, b, 1, 6, "READY_TO_SHIP", None).await;

    let (s, env) = scan_tree(&app, &token, "T-EMS-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let asm_node = &env["data"]["assembly"];
    assert_eq!(asm_node["quantity"], 10);
    assert_eq!(asm_node["entry_max_sets"], 2, "min(2, 2) = 2: {env}");
    let per_set = asm_node["per_set_parts"]
        .as_array()
        .expect("per_set_parts 必须是数组");
    assert_eq!(per_set.len(), 2);
    // 装配序按 part.id 升序
    let m: std::collections::HashMap<String, i64> = per_set
        .iter()
        .map(|p| {
            (
                p["part_id"].as_str().unwrap().to_string(),
                p["per_set_quantity"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(m.get(&a.to_string()), Some(&2), "25/10 向零截断 = 2");
    assert_eq!(m.get(&b.to_string()), Some(&3), "30/10 = 3");
}

/// `entry_max_sets` 量的是「**还能再入多少套**」，详情 VO 的 `shippable_sets` 量的是
/// 「**这一单能出多少套**」—— 同一份 `shippable_sets` 公式、同一子件集，但**分子不同**
/// （前者只取「可入单」批次 = `READY_TO_SHIP` + 未占用；后者只取「本单上」的批次）。
///
/// 因此二者**只在批次未占用时相等**；批次一旦被某张单挂上，扫码树报 0（已无可入单
/// 的货）而该单详情仍报它能出的套数。用例把两个场景都钉住，避免有人误把它们当同一个
/// 数字、或为了「对齐」而把其中一个改成另一个的口径。
#[tokio::test]
async fn entry_max_sets_is_entryable_based_not_note_based() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树两口径").await;
    let l2 = insert_l2(&pool, "树两口径二厂", l1).await;
    let asm = insert_assembly(&pool, "两口径体", "T-TWO-1", l2, 10).await;
    let a = insert_part(&pool, "两口径件A", "T-TWO-1-01", l2, Some(asm), 10).await;
    let b = insert_part(&pool, "两口径件B", "T-TWO-1-02", l2, Some(asm), 10).await;
    insert_batch(&pool, a, 1, 8, "READY_TO_SHIP", None).await;
    insert_batch(&pool, b, 1, 5, "READY_TO_SHIP", None).await;

    // 场景 1：批次未占用 ⇒ 两个口径同值 min(8, 5) = 5
    let (_, tree) = scan_tree(&app, &token, "T-TWO-1").await;
    assert_eq!(
        tree["data"]["assembly"]["entry_max_sets"], 5,
        "未占用: {tree}"
    );

    // 场景 2：批次挂到一张 DRAFT 单上 ⇒ 树报 0（无可入单），该单详情仍报 5
    let note_id = insert_draft_note(&pool, l1, "DN-TWO-0001").await;
    sqlx::query("UPDATE t_part_batch SET delivery_note_id = $1 WHERE part_id IN ($2, $3)")
        .bind(note_id)
        .bind(a)
        .bind(b)
        .execute(&pool)
        .await
        .expect("attach batches to note");

    let (_, tree2) = scan_tree(&app, &token, "T-TWO-1").await;
    assert_eq!(
        tree2["data"]["assembly"]["entry_max_sets"], 0,
        "批次已被占用 ⇒ 扫码树无可入单套数: {tree2}"
    );
    let (ds, detail) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/com/delivery/note/{note_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(ds, StatusCode::OK, "{detail}");
    assert_eq!(
        detail["data"]["line_items"][0]["shippable_sets"], 5,
        "本单详情仍报它能出的套数: {detail}"
    );
}

// ===========================================================================
//  5. draft 判定（纯读，不建单）
// ===========================================================================

/// 该 L1 名下有 DRAFT ⇒ `draft` 返回 `{note_id, note_no, version, status}`；没有 ⇒
/// `null`。**本端点绝不建单** —— 两种情况下 `t_delivery_note` 的行数都不变。
#[tokio::test]
async fn draft_reflects_existing_open_draft_and_never_creates() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树草稿").await;
    let l2 = insert_l2(&pool, "树草稿二厂", l1).await;
    insert_part(&pool, "草稿件", "T-DRF-1", l2, None, 5).await;

    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM t_delivery_note")
        .fetch_one(&pool)
        .await
        .expect("count");

    // 无 DRAFT ⇒ null，且零写入
    let (s1, env1) = scan_tree(&app, &token, "T-DRF-1").await;
    assert_eq!(s1, StatusCode::OK, "{env1}");
    assert!(env1["data"]["draft"].is_null(), "无草稿 ⇒ null: {env1}");
    let mid: i64 = sqlx::query_scalar("SELECT count(*) FROM t_delivery_note")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(mid, before, "扫码树端点绝不建单");

    // 建一张 DRAFT ⇒ 返回它（version 是 OCC 锚，前端回传给 POST /scan）
    let note_id = insert_draft_note(&pool, l1, "DN-DRF-0001").await;
    let (s2, env2) = scan_tree(&app, &token, "T-DRF-1").await;
    assert_eq!(s2, StatusCode::OK, "{env2}");
    assert_eq!(
        env2["data"]["draft"]["note_id"].as_str().unwrap(),
        note_id.to_string()
    );
    assert_eq!(env2["data"]["draft"]["note_no"], "DN-DRF-0001");
    assert_eq!(env2["data"]["draft"]["status"], "DRAFT");
    assert_eq!(env2["data"]["draft"]["version"], 3, "version 是 OCC 锚");

    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM t_delivery_note")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(after, before + 1, "只多了测试自己插的那一张");
}

/// 装配件树的 `draft` 用**装配件所属客户**上推的 L1（不是子件的 L2）。
#[tokio::test]
async fn draft_for_assembly_tree_uses_assembly_customer_l1() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树装配草稿").await;
    let l2 = insert_l2(&pool, "树装配草稿二厂", l1).await;
    let asm = insert_assembly(&pool, "装配草稿体", "T-DAS-1", l2, 4).await;
    insert_part(&pool, "装配草稿件", "T-DAS-1-01", l2, Some(asm), 8).await;
    let note_id = insert_draft_note(&pool, l1, "DN-DAS-0001").await;

    let (s, env) = scan_tree(&app, &token, "T-DAS-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["draft"]["note_id"].as_str().unwrap(),
        note_id.to_string()
    );
}

// ===========================================================================
//  6. 零 N+1 / 树完整性
// ===========================================================================

/// 一次请求返回整棵树的全部批次（子件 5 个 × 各 3 批 = 15 个批次一个不少）——
/// 钉住「批次层一条 `part_id = ANY($1)`」这条口径不被改成 per-part 查询后漏批次。
#[tokio::test]
async fn one_request_returns_every_batch_of_the_whole_tree() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树完整性").await;
    let l2 = insert_l2(&pool, "树完整性二厂", l1).await;
    let asm = insert_assembly(&pool, "完整性体", "T-FULL-1", l2, 5).await;
    let mut part_ids = Vec::new();
    for i in 1..=5 {
        let p = insert_part(
            &pool,
            &format!("完整性件{i}"),
            &format!("T-FULL-1-{i:02}"),
            l2,
            Some(asm),
            10,
        )
        .await;
        part_ids.push(p);
        for b in 1..=3 {
            insert_batch(&pool, p, b, 2, "READY_TO_SHIP", None).await;
        }
    }

    let (s, env) = scan_tree(&app, &token, "T-FULL-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let kids = env["data"]["children"].as_array().unwrap();
    assert_eq!(kids.len(), 5, "5 个子件都在");
    let total: usize = kids
        .iter()
        .map(|k| k["children"].as_array().unwrap().len())
        .sum();
    assert_eq!(total, 15, "15 个批次一个不少（不能因分批查询漏掉）");
    for p in &part_ids {
        assert_eq!(
            part_node(&env["data"], *p)["children"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
    assert_eq!(
        env["data"]["assembly"]["entry_max_sets"], 3,
        "每子件可入单 6 件 / 每套 2 件 = 3 套（受工单总套数 5 收口）: {env}"
    );
}

/// 装配件无活跃子件 ⇒ `children = []`（**不是** `null`），前端少一层 `?? []`。
#[tokio::test]
async fn assembly_without_children_returns_empty_array_not_null() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树空装配").await;
    let l2 = insert_l2(&pool, "树空装配二厂", l1).await;
    insert_assembly(&pool, "空装配体", "T-EMPTY-1", l2, 3).await;

    let (s, env) = scan_tree(&app, &token, "T-EMPTY-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["children"].as_array().unwrap().len(),
        0,
        "无子件 ⇒ 空数组而非 null: {env}"
    );
    assert_eq!(
        env["data"]["assembly"]["entry_max_sets"], 0,
        "无子件 ⇒ 0 套"
    );
}

/// 货架终端（`ShelfAccount`）访问扫码树 ⇒ 403（只该扫码核销，不该开送货单）。
#[tokio::test]
async fn scan_tree_rejects_shelf_account() {
    // ⚠️ 只装 part fixture：`load_delivery_fixture` 与 `load_part_fixture` 的
    // `t_customer` 常量 id 区段重叠，同库两次装会撞主键。
    let pool = test_pool().await;
    let part_fx = load_part_fixture(&pool).await;
    let l1 = insert_l1(&pool, "树权限").await;
    let l2 = insert_l2(&pool, "树权限二厂", l1).await;
    insert_part(&pool, "权限件", "T-ROLE-1", l2, None, 3).await;

    let app = test_app(test_state(pool.clone()).await);
    let shelf_token =
        login_token(&app, &part_fx.shelf_account_username, PartFixture::PASSWORD).await;
    let (s, env) = scan_tree(&app, &shelf_token, "T-ROLE-1").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "货架终端不该能读扫码树: {env}");
    assert_eq!(env["code"], 40300);
}

/// 序列化形态：i64 字段（part / assembly / batch / customer id）一律 JSON **string**。
#[tokio::test]
async fn all_ids_are_json_strings() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "树序列化").await;
    let l2 = insert_l2(&pool, "树序列化二厂", l1).await;
    let asm = insert_assembly(&pool, "序列化体", "T-JSON-1", l2, 5).await;
    let a = insert_part(&pool, "序列化件A", "T-JSON-1-01", l2, Some(asm), 10).await;
    let batch = insert_batch(&pool, a, 1, 3, "READY_TO_SHIP", None).await;

    let (s, env) = scan_tree(&app, &token, "T-JSON-1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let d = &env["data"];
    assert_eq!(d["assembly"]["id"], asm.to_string());
    assert_eq!(d["assembly"]["customer_id"], l2.to_string());
    assert_eq!(d["children"][0]["id"], a.to_string());
    assert_eq!(d["children"][0]["customer_id"], l2.to_string());
    assert_eq!(d["children"][0]["children"][0]["id"], batch.to_string());
    assert_eq!(
        d["children"][0]["children"][0]["version"], 0,
        "计数类仍是 JSON number"
    );
    let _ = json!(null); // 保持 json! 在 use 中（部分断言用不到时避免 unused 警告）
}
