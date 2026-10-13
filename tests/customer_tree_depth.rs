//! 客户树**任意深度**（2026-10-17 Gate C + Gate D）端到端集成测试。
//!
//! 客户从「最多二级」放开为任意深度后，本文件锁三条**行为契约**：
//!
//! 1. **`customer_path` 出参口径**（Gate C）：第 2 层到叶子用 `" / "` 拼接、
//!    **不含**根；`customer_name` = 本节点单段名；`l1_customer_name` =
//!    **根**名（不是「上跳一层」的直接父）。守 `POST /parts`（建单回读）与
//!    `GET /parts`（列表 enrichment）两条返回路径。
//! 2. **筛选展开 = 纯子树**（Gate D，行为变更）：选中某节点 = 该节点 + 其
//!    **任意深度**的全部后代，不含兄弟。
//!    - assembly 列表（旧 `expand_customer_l2_ids` 对非根节点**只返回自身**）
//!    - outsource 报价列表（旧 `part_ids_by_customer` 只展开**一层**）
//! 3. **业务单据只能挂叶子客户**（Gate D）：零件 / 装配件 / 外协报价的建单入口
//!    对「还有未软删子节点」的客户报 `20104 BIZ_INVALID_VALUE`，消息带客户路径与
//!    下级节点数。
//!
//! 为什么另起一个 binary 而不是并进 `tests/customer_api.rs`：本文件横跨
//! part / assembly / outsource 三个域的**端点行为**，`customer_api.rs` 守的是
//! customer 域自身的 CRUD 契约（上一轮 Gate A+B 已加 7 条）。
//!
//! ## 树形 fixture（全部用例复用同一形状）
//!
//! ```text
//! 华强(L1, prefix=H)
//! └─ 终端BG(L2)
//!    ├─ 手机部(L3)   ← 有子节点 ⇒ **非叶子**
//!    │  └─ 影像组(L4) ← 叶子
//!    └─ 显示BG(L3)   ← 手机部的兄弟；叶子
//! ```
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database，DB 间 schema 完全独立，无需 Mutex。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::modules::assembly::dto::{
    AssemblyChildRequest, AssemblyCreateRequest, AssemblyListQuery,
};
use hsh_erp_rust::modules::assembly::service::AssemblyService;
use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::*;

// ===========================================================================
//  Helpers
// ===========================================================================

fn next_test_id() -> i64 {
    shared_test_snowflake().next_id()
}

fn today() -> String {
    hsh_erp_rust::infra::clock::now_naive()
        .date()
        .format("%Y-%m-%d")
        .to_string()
}

/// 建一个客户节点。`parent = None` ⇒ 根（必须给 `serial_prefix`，DB CHECK 单个大写字母）。
async fn insert_customer(
    pool: &PgPool,
    name: &str,
    parent: Option<i64>,
    prefix: Option<&str>,
) -> i64 {
    let id = next_test_id();
    let now = hsh_erp_rust::infra::clock::now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(name)
    .bind(parent)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_customer");
    id
}

/// 上面那棵树。返回 `(l1, l2, l3_phone, l4_camera, l3_display)`。
async fn insert_tree(pool: &PgPool) -> (i64, i64, i64, i64, i64) {
    let l1 = insert_customer(pool, "华强", None, Some("H")).await;
    let l2 = insert_customer(pool, "终端BG", Some(l1), None).await;
    let l3_phone = insert_customer(pool, "手机部", Some(l2), None).await;
    let l4 = insert_customer(pool, "影像组", Some(l3_phone), None).await;
    let l3_display = insert_customer(pool, "显示BG", Some(l2), None).await;
    (l1, l2, l3_phone, l4, l3_display)
}

async fn insert_part(pool: &PgPool, customer_id: i64, tag: &str) -> i64 {
    let id = next_test_id();
    let now = hsh_erp_rust::infra::clock::now_naive();
    let day = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 1, 0, 0, $4, $4, $5, 'PENDING', 0, $6, $6)",
    )
    .bind(id)
    .bind(format!("PART-{tag}"))
    .bind(format!("DWG-{tag}-{id}"))
    .bind(day)
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 给本文件的树根（`serial_prefix = 'H'`）播一个序列号计数器 —— 根客户没配 counter
/// 时建单会被 20108（`serial counter for prefix 'H' not seeded`）先拒掉，测不到
/// 本来要测的东西。
async fn seed_serial_counter(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO t_serial_counter (prefix, counter) VALUES ('H', 0) \
         ON CONFLICT (prefix) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed t_serial_counter('H')");
}

async fn insert_outsource_process(pool: &PgPool, code_str: &str) -> i64 {
    let id = next_test_id();
    let now = hsh_erp_rust::infra::clock::now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code_str)
    .bind(format!("{code_str}-name"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

async fn insert_company(pool: &PgPool, name: &str) -> i64 {
    let id = next_test_id();
    let now = hsh_erp_rust::infra::clock::now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company (id, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, TRUE, 0, $3, $3)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company");
    id
}

fn manager_user() -> hsh_erp_rust::auth::rbac::CurrentUser {
    hsh_erp_rust::auth::rbac::CurrentUser {
        id: next_test_id(),
        username: "tree-manager".to_string(),
        roles: vec![hsh_erp_rust::auth::rbac::Role::Manager],
        permissions: vec![],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

fn list_query(customer_id: i64) -> AssemblyListQuery {
    AssemblyListQuery {
        customer_id: Some(customer_id.to_string()),
        status: None,
        statuses: None,
        is_urgent: None,
        keyword: None,
        sort_by: None,
        sort_dir: None,
        limit: None,
        offset: None,
    }
}

fn create_assembly_req(customer_id: i64, tag: &str) -> AssemblyCreateRequest {
    AssemblyCreateRequest {
        drawing_no: format!("ASM-{tag}"),
        name: format!("装配件-{tag}"),
        applicant_name: None,
        customer_id: customer_id.to_string(),
        request_date: None,
        planned_delivery_date: None,
        is_urgent: Some(false),
        quantity: Some(1),
        unit_price: None,
        total_price: None,
        order_no: None,
        system_delivery_date: None,
        note: None,
        children: Vec::<AssemblyChildRequest>::new(),
    }
}

/// 取响应 items 里 `customer_path` 的集合（断言用，顺序无关）。
///
/// 兼容两种形态：HTTP 信封（`{"data":{"items":[…]}}`）与 service 直调的裸 VO
/// （`{"items":[…]}`）—— 本文件的 assembly 用例走 service（建单是 multipart
/// 请求，见 `tests/assembly/create_serial_price.rs` 的 `create_body`）。
fn paths_of(env: &Value) -> Vec<String> {
    let items = env["data"]["items"]
        .as_array()
        .or_else(|| env["items"].as_array());
    let mut v: Vec<String> = items
        .expect("items 数组")
        .iter()
        .map(|it| it["customer_path"].as_str().unwrap_or_default().to_string())
        .collect();
    v.sort();
    v
}

fn part_names(env: &Value) -> Vec<String> {
    let mut v: Vec<String> = env["data"]["items"]
        .as_array()
        .expect("items 数组")
        .iter()
        .map(|it| it["part_name"].as_str().unwrap_or_default().to_string())
        .collect();
    v.sort();
    v
}

// ===========================================================================
//  1. customer_path 出参口径
// ===========================================================================

/// 4 层树的叶子客户上建单 ⇒ `customer_path` = `"终端BG / 手机部 / 影像组"`
/// （**不含**根「华强」），`customer_name` = 「影像组」，`l1_customer_name` = 「华强」。
///
/// 同时钉住 `POST /parts` 与 `GET /parts` 两条返回路径（建单回读 / 列表 enrichment）。
#[tokio::test]
async fn part_customer_path_columns_drop_the_root_segment() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    // 独立于 fixture 的 4 层树（fixture 的 L1/L2 会被别的用例复用，避免依赖它的形状）
    let (_l1, _l2, _l3, l4, _sib) = insert_tree(&pool).await;
    seed_serial_counter(&pool).await;
    let day = today();

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/parts",
            Some(json!({
                "name": "影像件",
                "drawing_no": "D-CAM-1",
                "applicant_name": "张三",
                "quantity": 1,
                "request_date": day,
                "planned_delivery_date": day,
                "is_urgent": false,
                "customer_id": l4.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "叶子客户建单应成功: {env}");
    assert_eq!(
        env["data"]["customer_path"], "终端BG / 手机部 / 影像组",
        "{env}"
    );
    assert_eq!(env["data"]["customer_name"], "影像组", "{env}");
    assert_eq!(env["data"]["l1_customer_name"], "华强", "{env}");

    // 列表路径（批量 enrichment 口径）必须与建单回读逐字一致
    let (s, env) = send(
        app,
        json_request("GET", "/parts?limit=50", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let items = env["data"]["items"].as_array().expect("items");
    let mine = items
        .iter()
        .find(|it| it["id"] == env_id_of(&env, "影像件"))
        .expect("刚建的行");
    assert_eq!(mine["customer_path"], "终端BG / 手机部 / 影像组", "{mine}");
    assert_eq!(mine["l1_customer_name"], "华强", "{mine}");
}

/// 取列表里第一个 `name == want` 的行 id（列表按 id DESC，故用名字定位而不是取第一个）。
fn env_id_of(env: &Value, want: &str) -> Value {
    env["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|it| it["name"] == want)
        .expect("按 name 找行")["id"]
        .clone()
}

// ===========================================================================
//  2. 叶子闸门：业务单据只能挂叶子客户
// ===========================================================================

/// `POST /parts` + `POST /parts/batch` 挂到**非叶子**客户 ⇒ 20104，且消息点名该客户
/// 与它下面的子节点数。
#[tokio::test]
async fn part_create_rejects_non_leaf_customer_with_20104() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    let (_l1, _l2, l3_phone, l4, _sib) = insert_tree(&pool).await;
    seed_serial_counter(&pool).await;
    let day = today();

    // ① 单件建单：手机部下面还有影像组 ⇒ 非叶子
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/parts",
            Some(json!({
                "name": "挂中间层",
                "drawing_no": "D-MID-1",
                "applicant_name": "张三",
                "quantity": 1,
                "request_date": day,
                "planned_delivery_date": day,
                "is_urgent": false,
                "customer_id": l3_phone.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "非叶子客户建单应 400: {env}");
    assert_eq!(
        env["code"].as_i64(),
        Some(code::BIZ_INVALID_VALUE.into()),
        "{env}"
    );
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(msg.contains("叶子"), "消息应点名「叶子」: {msg}");
    assert!(msg.contains("手机部"), "消息应点名该客户: {msg}");
    assert!(msg.contains("1 个子节点"), "消息应给出下级节点数: {msg}");

    // ② 批量建单走同一条闸门（legacy 路径 = handler 无文件绑定时的那条）
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/parts/batch",
            Some(json!({
                "customer_id": l3_phone.to_string(),
                "items": [{
                    "name": "批件",
                    "drawing_no": "D-BATCH-MID",
                    "applicant_name": "张三",
                    "quantity": 1,
                    "request_date": day,
                    "planned_delivery_date": day,
                    "is_urgent": false,
                }],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "批量建单非叶子应 400: {env}");
    assert_eq!(
        env["code"].as_i64(),
        Some(code::BIZ_INVALID_VALUE.into()),
        "{env}"
    );

    // ③ 叶子客户（影像组）必须放行 —— 闸门不是「非根即拒」
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts",
            Some(json!({
                "name": "挂叶子",
                "drawing_no": "D-LEAF-1",
                "applicant_name": "张三",
                "quantity": 1,
                "request_date": day,
                "planned_delivery_date": day,
                "is_urgent": false,
                "customer_id": l4.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "叶子客户应放行: {env}");
}

/// 装配件建单 / 改客户 挂非叶子 ⇒ 20104；叶子 ⇒ 成功。
///
/// 「改客户也守叶子」是刻意同口径：改单是唯一能把已建装配件挪到非叶子客户上的入口，
/// 建单守得住而改单守不住 ⇒ 闸门形同虚设。
#[tokio::test]
async fn assembly_create_and_update_reject_non_leaf_customer() {
    let pool = test_pool().await;
    let fx = load_assembly_fixture(&pool).await;
    let (_l1, _l2, l3_phone, l4, _sib) = insert_tree(&pool).await;
    seed_serial_counter(&pool).await;
    let _ = &fx;

    let current = manager_user();

    // ① 非叶子 ⇒ 20104
    let mut tx = pool.begin().await.unwrap();
    let err = AssemblyService::create_assembly(
        &mut tx,
        shared_test_snowflake().as_ref(),
        &create_assembly_req(l3_phone, "MID"),
        vec![],
        &current,
    )
    .await
    .expect_err("非叶子客户建装配件应被拒");
    assert_eq!(err.code(), code::BIZ_INVALID_VALUE, "{err}");
    drop(tx);

    // ② 叶子 ⇒ 成功
    let mut tx = pool.begin().await.unwrap();
    let out = AssemblyService::create_assembly(
        &mut tx,
        shared_test_snowflake().as_ref(),
        &create_assembly_req(l4, "LEAF"),
        vec![],
        &current,
    )
    .await
    .expect("叶子客户建装配件应成功");
    tx.commit().await.unwrap();
    let assembly_id = out.assembly.id;

    // ③ 改客户到非叶子 ⇒ 20104
    let upd = hsh_erp_rust::modules::assembly::dto::AssemblyUpdateRequest {
        version: out.assembly.version,
        customer_id: Some(Some(l3_phone.to_string())),
        ..Default::default()
    };
    let mut tx = pool.begin().await.unwrap();
    let err = AssemblyService::update_assembly(&mut tx, assembly_id, &upd, &current)
        .await
        .expect_err("改到非叶子客户应被拒");
    assert_eq!(err.code(), code::BIZ_INVALID_VALUE, "{err}");
    tx.rollback().await.unwrap();
}

/// 外协报价建单挂到非叶子客户的零件 ⇒ 20104。
#[tokio::test]
async fn outsource_quote_create_rejects_part_on_non_leaf_customer() {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;

    let (_l1, _l2, l3_phone, _l4, _sib) = insert_tree(&pool).await;
    let part_id = insert_part(&pool, l3_phone, "NONLEAF").await;
    let company = insert_company(&pool, "非叶子报价公司").await;
    let proc_id = insert_outsource_process(&pool, "NLPROC").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": part_id.to_string(),
                "outsource_company_id": company.to_string(),
                "process_id": proc_id.to_string(),
                "price": "3.00",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(
        env["code"].as_i64(),
        Some(code::BIZ_INVALID_VALUE.into()),
        "{env}"
    );
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("手机部"),
        "消息应点名该客户: {env}"
    );
}

// ===========================================================================
//  3. 筛选展开 = 纯子树（Gate D 行为变更的回归锁）
// ===========================================================================

/// assembly 列表：选中非根节点 ⇒ 自身 + 其**任意深度**的全部后代。
///
/// 旧实现的 `expand_customer_l2_ids` 对**非根节点只返回自身**（「L2 必然是叶子」
/// 是两层时代的假设），故「选中第 2 层看不到它第 3 层建的装配件」正是本用例要
/// 钉住的行为变更。
///
/// 树形（三台装配件挂在**三个不同深度的叶子**上 —— 叶子闸门决定了中间层不能挂单）：
///
/// ```text
/// 华强(根) ─┬→ 终端BG ─┬→ 手机部(叶)     ← 装配件 A
///           │           └→ 显示BG(叶)   ← 装配件 B
///           └→ 影像线 ───→ 相机组(叶)   ← 装配件 C（根之下第 3 层）
/// ```
#[tokio::test]
async fn assembly_list_filter_expands_to_pure_subtree() {
    let pool = test_pool().await;
    let fx = load_assembly_fixture(&pool).await;
    seed_serial_counter(&pool).await;
    let _ = &fx;

    let l1 = insert_customer(&pool, "华强", None, Some("H")).await;
    let l2 = insert_customer(&pool, "终端BG", Some(l1), None).await;
    let phone = insert_customer(&pool, "手机部", Some(l2), None).await;
    let display = insert_customer(&pool, "显示BG", Some(l2), None).await;
    let l2_cam = insert_customer(&pool, "影像线", Some(l1), None).await;
    let camera = insert_customer(&pool, "相机组", Some(l2_cam), None).await;

    let current = manager_user();
    for (cid, tag) in [(phone, "PHONE"), (display, "DISP"), (camera, "CAM")] {
        let mut tx = pool.begin().await.unwrap();
        AssemblyService::create_assembly(
            &mut tx,
            shared_test_snowflake().as_ref(),
            &create_assembly_req(cid, tag),
            vec![],
            &current,
        )
        .await
        .expect("seed assembly");
        tx.commit().await.unwrap();
    }

    // 选第 2 层（终端BG）⇒ 它两个叶子上的装配件。**旧实现这里只返自身 ⇒ 0 行**
    let mut tx = pool.begin().await.unwrap();
    let out = AssemblyService::list_assemblies(&mut tx, &list_query(l2), &current)
        .await
        .expect("list 第 2 层");
    drop(tx);
    assert_eq!(out.total, 2, "第 2 层子树 = 两个叶子: {out:?}");
    assert_eq!(
        paths_of(&serde_json::to_value(&out).unwrap()),
        vec!["终端BG / 手机部", "终端BG / 显示BG",]
    );

    // 选根 ⇒ 三台（含根之下**第 3 层**的相机组）
    let mut tx = pool.begin().await.unwrap();
    let out = AssemblyService::list_assemblies(&mut tx, &list_query(l1), &current)
        .await
        .expect("list 根");
    drop(tx);
    assert_eq!(out.total, 3, "根子树含第 2 / 第 3 层叶子: {out:?}");
    assert_eq!(
        paths_of(&serde_json::to_value(&out).unwrap()),
        vec!["影像线 / 相机组", "终端BG / 手机部", "终端BG / 显示BG",]
    );

    // 选叶子（手机部）⇒ 只有自己一台（兄弟 显示BG 不进结果集）
    let mut tx = pool.begin().await.unwrap();
    let out = AssemblyService::list_assemblies(&mut tx, &list_query(phone), &current)
        .await
        .expect("list 手机部");
    drop(tx);
    assert_eq!(out.total, 1, "兄弟子树互不串味: {out:?}");
    assert_eq!(
        paths_of(&serde_json::to_value(&out).unwrap()),
        vec!["终端BG / 手机部",]
    );
}

/// 外协报价列表：选中某个节点 ⇒ 命中其**任意深度**后代的零件报价。
///
/// 旧 `part_ids_by_customer` 的谓词是「`customer_id = $1 OR customer_id IN (直接
/// 子客户)`」—— 只展开一层。本用例的零件全部挂在**两层之下**的叶子上（叶子闸门
/// 的直接后果：单据只能挂叶子），所以旧谓词对根 / 中间层一律返 0 行，新谓词返 2 行。
///
/// 同时钉住兄弟子树互不串味（选中 leaf_a 只返它自己）与 `customer_path` 出参。
#[tokio::test]
async fn outsource_quote_list_filter_expands_to_pure_subtree() {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;

    // 三级树 A：报价根 → 报价中间层 → {leaf_a, leaf_b}（两个叶子，互为兄弟）
    let root = insert_customer(&pool, "报价根", None, Some("W")).await;
    let mid = insert_customer(&pool, "报价中间层", Some(root), None).await;
    let leaf_a = insert_customer(&pool, "报价叶A", Some(mid), None).await;
    let leaf_b = insert_customer(&pool, "报价叶B", Some(mid), None).await;
    // 另一棵树的叶子：不在本子树内
    let other = insert_customer(&pool, "别树叶子", None, Some("V")).await;

    let company = insert_company(&pool, "子树报价公司").await;
    let proc_id = insert_outsource_process(&pool, "SUBTREEPROC").await;
    // 报价只能建在**叶子**客户的零件上（Gate D 叶子闸门）⇒ 三个零件全挂叶子
    for (cid, tag) in [(leaf_a, "LEAF-A"), (leaf_b, "LEAF-B"), (other, "OTHER")] {
        let part_id = insert_part(&pool, cid, tag).await;
        let (s, env) = send(
            app.clone(),
            json_request(
                "POST",
                "/outsource-quotes",
                Some(json!({
                    "part_id": part_id.to_string(),
                    "outsource_company_id": company.to_string(),
                    "process_id": proc_id.to_string(),
                    "price": "3.00",
                })),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "建报价: {env}");
    }

    let get = |qs: String, app: axum::Router, token: String| async move {
        send(
            app.clone(),
            json_request("GET", &format!("/outsource-quotes{qs}"), None, Some(&token)),
        )
        .await
    };

    // 选根 ⇒ 命中**两层之下**的两个叶子（旧谓词只展开一层 ⇒ 这里是 0 行）
    let (s, env) = get(format!("?customer_id={root}"), app.clone(), token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "根的子树含两层下的叶子零件: {env}");
    assert_eq!(
        part_names(&env),
        vec!["PART-LEAF-A".to_string(), "PART-LEAF-B".to_string()],
        "{env}"
    );

    // 选中层 ⇒ 同上（自身 + 后代）
    let (s, env) = get(format!("?customer_id={mid}"), app.clone(), token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "{env}");

    // 选中叶子 ⇒ 只有自己一个（兄弟不进结果集）
    let (s, env) = get(format!("?customer_id={leaf_a}"), app.clone(), token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "兄弟不进结果集: {env}");
    // customer_path 直接读 paths_of 的 path：第 2 层到叶子、**不含根**
    assert_eq!(
        env["data"]["items"][0]["customer_path"], "报价中间层 / 报价叶A",
        "{env}"
    );

    // 别树不在本子树内
    let (s, env) = get(format!("?customer_id={other}"), app, token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
}

// ===========================================================================
//  4. Gate E（2026-10-17）：前两轮漏掉的三个域的 `customer_path`
// ===========================================================================

/// `prod::inspection` 待品检队列：行项三列（自身名 / **根**名 / 第 2 层到叶子的路径）。
///
/// 改因：Gate C 只给 part / assembly / outsource / queue / dashboard 补了
/// `customer_path`，inspection 队列的 `l1_customer_name` 仍是 SQL 自连 `c.parent_id`
/// 取到的**一层父名** —— 四层树上它会是「手机部」而不是根「华强」。新口径由
/// `shared::customer::paths_of` 一次批量产出（service 层），SQL 侧的自连已删。
#[tokio::test]
async fn inspection_queue_customer_columns_on_four_level_tree() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    let (_l1, _l2, _l3, l4, _sib) = insert_tree(&pool).await;

    // 直接插一条 INSPECTION 批次（待品检队列只筛 `status='INSPECTION'`）
    let part_id = insert_part(&pool, l4, "QUEUE").await;
    let batch_id = next_test_id();
    let now = hsh_erp_rust::infra::clock::now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) VALUES ($1, $2, 1, 3, 'INSPECTION', 0, $3, $3)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert INSPECTION batch");

    let (s, env) = send(
        app,
        json_request("GET", "/prod/inspection/queue?limit=50", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let part_id_s = part_id.to_string();
    let mine = env["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        // `part_id` 在 wire 上是 JSON string；先备好 `&str` 再比，避免每次比较都
        // 新建一个 `String`（clippy::cmp_owned）
        .find(|it| it["part_id"].as_str() == Some(part_id_s.as_str()))
        .expect("刚插的行");
    assert_eq!(mine["customer_name"], "影像组", "{mine}");
    assert_eq!(
        mine["l1_customer_name"], "华强",
        "根名（旧的「一层父名」口径在四层树上会给「手机部」）: {mine}"
    );
    assert_eq!(
        mine["customer_path"], "终端BG / 手机部 / 影像组",
        "第 2 层到叶子、不含根: {mine}"
    );
}

/// `prod::inspection` 扫码树：装配件节点与零件节点的 `customer_path`。
#[tokio::test]
async fn inspection_scan_tree_customer_path_on_four_level_tree() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    let (_l1, _l2, _l3, l4, _sib) = insert_tree(&pool).await;
    // 本文件的 `insert_part` 不写 serial_no，扫码端点按 serial_no 命中 ⇒ 这里直插。
    // ⚠️ `t_part.serial_no` 是 `varchar(15)`，故用短码而不是把雪花 id 拼进去。
    let part_id = next_test_id();
    let serial_no = format!("PS{}", &part_id.to_string()[15..]);
    let now = hsh_erp_rust::infra::clock::now_naive();
    let day = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, 'PART-SCAN-TREE', 'DWG-SCAN', '张三', 1, $3, $3, $4, 'PENDING', 0, $5, $5)",
    )
    .bind(part_id)
    .bind(serial_no.clone())
    .bind(day)
    .bind(l4)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_part with serial_no");

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/inspection/scan/{serial_no}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["hit_kind"], "PART", "{env}");
    assert_eq!(
        env["data"]["children"][0]["customer_path"], "终端BG / 手机部 / 影像组",
        "{env}"
    );
    assert_eq!(
        env["data"]["children"][0]["customer_name"], "影像组",
        "{env}"
    );
}

/// `prod::programming` 待编程一览：`parent_customer_name` = 根名 + 新增 `customer_path`。
#[tokio::test]
async fn programming_pending_customer_columns_on_four_level_tree() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    let (_l1, _l2, _l3, l4, _sib) = insert_tree(&pool).await;
    // 待编程一览的状态闸门是 `PENDING/IN_PROCESS/PROGRAMMING` 且命中三规则之一；
    // 最省的一条是直接置 PROGRAMMING（规则 1）。
    let part_id = next_test_id();
    let now = hsh_erp_rust::infra::clock::now_naive();
    let day = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, 'PART-PROG', 'DWG-PROG', '张三', 1, $2, $2, $3, 'PROGRAMMING', 0, $4, $4)",
    )
    .bind(part_id)
    .bind(day)
    .bind(l4)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_part PROGRAMMING");

    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/prod/programming/pending?limit=50",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let part_id_s = part_id.to_string();
    let mine = env["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        // 同上：`id` 是 JSON string，先备 `&str`
        .find(|it| it["id"].as_str() == Some(part_id_s.as_str()))
        .expect("刚插的行");
    assert_eq!(mine["customer_name"], "影像组", "{mine}");
    assert_eq!(
        mine["parent_customer_name"], "华强",
        "根名（旧口径是 SQL 自连取到的一层父名 = 手机部）: {mine}"
    );
    assert_eq!(mine["customer_path"], "终端BG / 手机部 / 影像组", "{mine}");
}

/// `assembly` 详情 / 建单回读也带上 `customer_path`（Gate C 只加到了列表 VO）。
///
/// 改因：上一轮的回执说明是「`render_assembly_out` 拿不到连接，加过去只能恒空串」。
/// 本轮新增 `assembly_out_with_path`（六个调用点都持有 repo），故该缺口关闭。
/// ⚠️ wire 形状对列表**不变**：`customer_path` 从 `AssemblyListItem` 移到
/// `AssemblyOut`（flatten 平铺），同一个键、同一个值。
#[tokio::test]
async fn assembly_out_carries_customer_path_on_four_level_tree() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    let (_l1, _l2, _l3, l4, _sib) = insert_tree(&pool).await;
    seed_serial_counter(&pool).await;

    let current = manager_user();
    let mut tx = pool.begin().await.expect("begin");
    let created = AssemblyService::create_assembly(
        &mut tx,
        shared_test_snowflake().as_ref(),
        &create_assembly_req(l4, "GATE-E"),
        vec![],
        &current,
    )
    .await
    .expect("叶子客户建装配件应成功");
    let asm_id: i64 = created.assembly.id;
    // 建单回读（`AssemblyCreateResult.assembly`）已带路径
    let created_json = serde_json::to_value(&created.assembly).expect("serialize");
    assert_eq!(
        created_json["customer_path"], "终端BG / 手机部 / 影像组",
        "建单回读出参必须带 customer_path: {created_json}"
    );
    tx.commit().await.expect("commit");

    // 详情（`AssemblyDetail` 把 head 平铺到顶层）
    let mut conn = pool.acquire().await.expect("acquire");
    let detail = AssemblyService::get_assembly(&mut conn, asm_id, &current)
        .await
        .expect("get assembly");
    let detail_json = serde_json::to_value(&detail).expect("serialize");
    assert_eq!(
        detail_json["customer_path"], "终端BG / 手机部 / 影像组",
        "详情出参必须带 customer_path: {detail_json}"
    );
}
