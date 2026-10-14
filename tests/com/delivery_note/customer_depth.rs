//! delivery_note 域 × 客户树**任意深度**（2026-10-17 Gate E）端到端集成测试。
//!
//! 覆盖本轮（Gate E）对 `com::delivery_note` 的五处改动，每条一个用例：
//!
//! 1. **分组成员由「L1 的直接子」放宽为「任意后代」**（`validate_group_members`）——
//!    L4 节点能入组、L1 之外的根不能入组；`ungrouped_customers` 列出**全部后代**。
//! 2. **扫码入单的叶子闸门**：扫进来的零件挂在**有子节点**的客户上 ⇒ 20104，且零写入。
//! 3. **`l1_of` 上溯到根**：四层树上两个 L4 分属同一 L1 ⇒ 共用同一张 DRAFT
//!    （旧实现 `parent_id.unwrap_or(id)` 只上跳一层，会把同集团拆成两张单）。
//! 4. **`customer_path` = 「第 2 层到叶子」拼接**（不含根）+ `parent_customer_name`
//!    = **根**名（旧口径是 `{直接父} / {自身}` 两段拼接）。
//! 5. **单据头的 `customer_path`**：送货单恒挂 L1 根 ⇒ 该字段 = 根自己的名字。
//!
//! ## 树形（全部用例复用 `load_customer_tree_fixture`）
//!
//! ```text
//! FX 深层集团(L1, prefix=Q)
//! └─ FX 深层事业群(L2)                ← 中间层
//!    ├─ FX 深层事业部(L3)             ← 中间层
//!    │  ├─ FX 深层车间(L4)            ← 叶子
//!    │  └─ FX 深层班组(L4)            ← 叶子
//!    └─ FX 深层分部(L3)               ← 叶子
//! ```
//!
//! ## 与其它测试文件的关系
//! part / assembly / outsource 三域的「任意深度」行为在 `tests/customer_tree_depth.rs`
//! （Gate C+D）；本文件只管 delivery_note 这一个域，与 `entry_gate.rs`（21405 /
//! 21406 / 21407 闸门）、`group.rs`（分组 CRUD 基线）、`scan_tree.rs`（三层树形状）
//! **不重叠**：那三个文件仍只造两层树。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_test_support::{
    CustomerTreeFixture, DeliveryFixture, json_request, load_customer_tree_fixture,
    load_delivery_fixture, login_token, pool_snowflake, send, test_app, test_pool, test_state,
};

fn next_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
}

/// fresh DB + delivery fixture（MANAGER 用户）+ 深树 fixture + 已登录 token。
async fn bootstrap() -> (
    PgPool,
    axum::Router,
    String,
    DeliveryFixture,
    CustomerTreeFixture,
) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let tree = load_customer_tree_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token, fx, tree)
}

/// 直插一个客户节点（`parent = None` ⇒ 根）。
async fn insert_customer(
    pool: &PgPool,
    name: &str,
    parent: Option<i64>,
    prefix: Option<&str>,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 0, $5, NULL, $5, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(parent)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert customer");
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
         VALUES ($1, $2, $3, 'D-DEPTH', $4, 'READY_TO_SHIP', '深树测试', $5, $5, 20, 0, \
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

async fn insert_batch(pool: &PgPool, part_id: i64, quantity: i32) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 'READY_TO_SHIP', 'INSPECTION_SHELF', 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(part_id)
    .bind(quantity)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

async fn part_entry(part_id: i64, quantity: i32) -> Value {
    json!([{"node_kind": "PART", "node_id": part_id.to_string(), "quantity": quantity}])
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

async fn get_json(app: &axum::Router, token: &str, path: &str) -> (StatusCode, Value) {
    send(app.clone(), json_request("GET", path, None, Some(token))).await
}

/// 建单后读回详情（送货单 `customer_id` 恒为 L1 根 ⇒ 详情头即根）。
async fn scan_tree(app: &axum::Router, token: &str, serial_no: &str) -> Value {
    let (s, env) = get_json(
        app,
        token,
        &format!("/com/delivery/note/scan/{}", serial_no),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan_tree 应 200: {env}");
    env
}

// ===========================================================================
//  1. 分组成员：任意后代可入组
// ===========================================================================

/// L4（深 4 层）节点可以入组，且 `ungrouped_customers` 列出该根的**全部后代**。
///
/// 改因：本用例断言的是**旧口径作废**后的新行为 —— 旧 `validate_l2_members` 要求
/// 「成员必须是 L1 的**直接子**」（`customer.parent_id == l1_id`），L4 节点会被
/// 20104 拒掉；`list_for_l1` 的 `ungrouped_customers` 也只列 `list_children`
/// （直接子），L3/L4 在候选里根本看不到。新口径：成员 = 该根的任意后代，
/// 候选 = 全部后代里未入组的。
#[tokio::test]
async fn group_member_may_be_any_depth_descendant() {
    let (pool, app, token, _fx, tree) = bootstrap().await;

    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": tree.tree_l1_id.to_string(),
                "name": "深树分组",
                // L4 叶子 + L3 中间层：两种深度都必须能入组
                "member_customer_ids": [
                    tree.tree_l4_a_id.to_string(),
                    tree.tree_l3_b_id.to_string(),
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "L4 + L3 都应是合法成员: {env1}");
    let members = env1["data"]["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{env1}");
    let names: Vec<&str> = members
        .iter()
        .map(|m| m["customer_name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"FX 深层车间") && names.contains(&"FX 深层分部"),
        "成员显示名应取自身单段名（深层节点不能显示成「(已删除)」）: {env1}"
    );

    // list：成员名正确 + 候选集含未入组的深层节点（L2 / L3-A / L4-B 三个）
    let (s2, env2) = get_json(
        &app,
        &token,
        &format!("/com/delivery/group?customer_id={}", tree.tree_l1_id),
    )
    .await;

    assert_eq!(s2, StatusCode::OK, "{env2}");
    let ungrouped = env2["data"]["ungrouped_customers"].as_array().unwrap();
    let ungrouped_names: Vec<&str> = ungrouped
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for expect in ["FX 深层事业群", "FX 深层事业部", "FX 深层班组"] {
        assert!(
            ungrouped_names.contains(&expect),
            "未分组候选应含 {expect}（旧口径只列直接子，深层节点看不见）: {env2}"
        );
    }
    for gone in ["FX 深层车间", "FX 深层分部"] {
        assert!(
            !ungrouped_names.contains(&gone),
            "已入组的 {gone} 不该再出现在候选里: {env2}"
        );
    }

    drop(pool);
}

/// 另一个根的后代**不能**进本根的组（20104），根自身也不能当自己的成员。
#[tokio::test]
async fn group_member_from_other_root_rejected_20104() {
    let (pool, app, token, _fx, tree) = bootstrap().await;
    let other_l1 = insert_customer(&pool, "别的集团", None, Some("X")).await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": tree.tree_l1_id.to_string(),
                "name": "越权分组",
                "member_customer_ids": [other_l1.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20104, "{env}");
    assert!(
        env["message"].as_str().unwrap().contains("后代"),
        "错误消息应说明成员必须是该根的后代: {env}"
    );

    // 根自身（= 分组键）不是自己的后代
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": tree.tree_l1_id.to_string(),
                "name": "自组",
                "member_customer_ids": [tree.tree_l1_id.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::BAD_REQUEST, "{env2}");
    assert_eq!(env2["code"], 20104, "分组键（根）不是自己的后代: {env2}");
}

// ===========================================================================
//  2. 扫码入单的叶子闸门
// ===========================================================================

/// 零件挂在**有子节点**的客户上 ⇒ 20104，且零写入（不建草稿、不挂批次）。
///
/// 改因：本轮新增的闸门。旧口径没有这道检查，挂在中间层客户的零件可以照常入单，
/// 而送货凭证上的客户名会是个「还有下级」的组织节点，不是收货单位。
#[tokio::test]
async fn scan_entry_on_non_leaf_customer_rejected_20104() {
    let (pool, app, token, _fx, tree) = bootstrap().await;
    // `tree_l3_a_id` 下面挂着两个 L4 ⇒ 非叶子
    let part_id = insert_part(&pool, "中间层零件", "DEPTH-NONLEAF-1", tree.tree_l3_a_id).await;
    insert_batch(&pool, part_id, 10).await;

    let (s, env) = scan_entry(
        &app,
        &token,
        "DEPTH-NONLEAF-1",
        part_entry(part_id, 5).await,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20104, "非叶子客户 ⇒ 20104: {env}");
    let msg = env["message"].as_str().unwrap();
    assert!(msg.contains("叶子"), "{env}");
    assert!(
        msg.contains("FX 深层事业群 / FX 深层事业部"),
        "消息应带客户路径（第 2 层到叶子，不含根）: {env}"
    );

    // 零写入：闸门在建单之前 ⇒ 既没有草稿也没有挂单批次
    let notes: i64 = sqlx::query_scalar("SELECT count(*) FROM t_delivery_note")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(notes, 0, "被拒时不得凭空建出草稿: {env}");
    let attached: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_part_batch WHERE delivery_note_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attached, 0, "被拒时不得挂任何批次: {env}");
}

/// 挂在**叶子**客户上的零件照常入单（闸门不能误伤正常路径）。
#[tokio::test]
async fn scan_entry_on_leaf_customer_ok() {
    let (pool, app, token, _fx, tree) = bootstrap().await;
    let part_id = insert_part(&pool, "叶子零件", "DEPTH-LEAF-1", tree.tree_l4_a_id).await;
    insert_batch(&pool, part_id, 10).await;

    let (s, env) = scan_entry(&app, &token, "DEPTH-LEAF-1", part_entry(part_id, 5).await).await;
    assert_eq!(s, StatusCode::OK, "叶子客户应放行: {env}");
}

// ===========================================================================
//  3. l1_of 上溯到根 + 4. customer_path
// ===========================================================================

/// 四层树上：两个 L4 分属同一 L1 ⇒ 扫码树给出**同一张** DRAFT（不按中间层拆单）。
///
/// 改因：旧 `l1_of` 是 `parent_id.unwrap_or(id)`（**只上跳一层**）⇒ L4 零件拿到它的
/// L3 父当「L1」，同集团的两个事业部会被拆成两张送货单、编号前缀也派错（中间层的
/// `serial_prefix` 恒 NULL）。新口径走 `shared::customer::root_id_of`。
#[tokio::test]
async fn scan_tree_draft_resolves_root_on_four_level_tree() {
    let (pool, app, token, _fx, tree) = bootstrap().await;
    let p1 = insert_part(&pool, "车间件", "DEPTH-A1", tree.tree_l4_a_id).await;
    insert_batch(&pool, p1, 5).await;
    let p2 = insert_part(&pool, "班组件", "DEPTH-A2", tree.tree_l4_b_id).await;
    insert_batch(&pool, p2, 5).await;

    // 先扫 A1 建草稿
    let (s, env) = scan_entry(&app, &token, "DEPTH-A1", part_entry(p1, 1).await).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    // `DeliveryNoteDetailOut` 用 `#[serde(flatten)]` 把 head 平铺到顶层 ⇒ 取 `data.id`
    let note_id = env["data"]["id"].as_str().unwrap().to_string();

    // 再扫 A2（同 L1、不同 L3 兄弟分支）：应命中**同一张**草稿
    let tree_b = scan_tree(&app, &token, "DEPTH-A2").await;
    assert_eq!(
        tree_b["data"]["draft"]["note_id"].as_str().unwrap(),
        note_id,
        "同 L1 的两个 L4 必须共用一张 DRAFT（只上跳一层会把中间层当根）: {tree_b}"
    );

    // 草稿行的 customer_id 恒为**根**
    let persisted: i64 = sqlx::query_scalar("SELECT customer_id FROM t_delivery_note")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        persisted, tree.tree_l1_id,
        "送货单必须挂 L1 根（分组键 / 编号前缀键 / 唯一索引都挂在根上）"
    );
}

/// 行项三列：自身名 / **根**名 / 「第 2 层到叶子」的路径（不含根）；单据头 = 根自己名字。
///
/// 改因：断言的是**旧口径作废**后的新行为 —— 旧实现是「查两张 `t_customer`（零件自身
/// 客户 + 其**直接父**）再 `format!("{父名} / {自身名}")`」，在四层树上必然只给两段
/// （`FX 深层事业部 / FX 深层车间`，丢掉 `FX 深层事业群` 与根），且
/// `parent_customer_name` 拿到的是中间层名而不是集团名。
#[tokio::test]
async fn delivery_note_customer_path_is_level2_to_leaf() {
    let (pool, app, token, _fx, tree) = bootstrap().await;
    let part_id = insert_part(&pool, "路径零件", "DEPTH-PATH-1", tree.tree_l4_a_id).await;
    insert_batch(&pool, part_id, 4).await;

    let (s, env) = scan_entry(&app, &token, "DEPTH-PATH-1", part_entry(part_id, 2).await).await;
    assert_eq!(s, StatusCode::OK, "{env}");

    // 单据头（已平铺到 `data` 顶层）：送货单挂根 ⇒ customer_path = 根自己的名字
    assert_eq!(
        env["data"]["customer_path"], "FX 深层集团",
        "单据恒挂 L1 根 ⇒ 路径 = 根自己的名字: {env}"
    );
    assert_eq!(env["data"]["customer_name"], "FX 深层集团", "{env}");

    let item = &env["data"]["line_items"][0];
    assert_eq!(item["customer_name"], "FX 深层车间", "本节点单段名: {env}");
    assert_eq!(
        item["parent_customer_name"], "FX 深层集团",
        "parent_customer_name 是**根**名（旧口径是直接父名 = FX 深层事业部）: {env}"
    );
    assert_eq!(
        item["customer_path"], "FX 深层事业群 / FX 深层事业部 / FX 深层车间",
        "第 2 层到叶子拼接、不含根（旧口径只给两段）: {env}"
    );
    assert_eq!(
        item["customer_id"].as_str().unwrap(),
        tree.tree_l4_a_id.to_string(),
        "行项的 customer_id 是叶子客户 id（分组打印键）: {env}"
    );
}
