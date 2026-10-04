//! `seeds/menu.sql` 授权矩阵回归测试（2026-10-05 新增）
//!
//! 钉住 commit `420652b`（「2026-10-05 收紧生产管理菜单可见性」）的授权口径，
//! 防止将来有人把 code 悄悄加回 `seeds/menu.sql` 第 4 节白名单、或删掉第 4.7 段
//! 回收区段。**一行 Rust 产品代码都不改**——纯 seed 可见性变更，回归护栏只落测试。
//!
//! ## 用户需求原文（2026-10-05）
//!
//! 「收紧权限，生产管理下的工序工种、制定工序菜单需要 MANAGER，生产队列菜单需要
//! CLERK，品检看不到这三个菜单才对」
//!
//! | menuCode          | 菜单   | MANAGER | CLERK | INSPECTOR |
//! |-------------------|--------|:-------:|:-----:|:---------:|
//! | `process_work_type` | 工序工种 | 保留 | 收回 | 收回 |
//! | `part_process_chain`| 制定工序 | 保留 | 收回 | 收回 |
//! | `worker_queue`     | 生产队列 | 保留 | 保留 | 收回 |
//!
//! `production_group`（生产管理分组）在 MANAGER / CLERK / INSPECTOR 三方都必须保留：
//! INSPECTOR 靠它挂 `inspection_pending`（待品检），CLERK 靠它挂 `worker_queue`。
//!
//! ## 4 个用例各钉什么
//!
//! 1. **`menu_seed_role_matrix`** —— 钉住 `t_role_menu` 授权口径的全量快照
//!    （每角色 code 集合**完全相等**，不是「包含」这种弱断言）。
//! 2. **`menu_seed_revokes_legacy_grants`** —— 钉住「回收作用于存量行」。这是最关键的
//!    一个用例：`seeds/menu.sql` 第 4 节的授权写入是**纯增量**
//!    （`ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING`），
//!    **从白名单里删 code 并不会回收生产库已存在的授权行**——所以才有第 4.7 段显式
//!    软删回收。若这条用例缺席，将来有人删掉 4.7 段、把 code 加回白名单，测试全绿
//!    而线上菜单没收紧。
//! 3. **`menu_seed_idempotent`** —— 钉住幂等：重跑不产生重复 live 行、可见矩阵逐次
//!    不变、且已被回收的软删行 `version` **不再被 bump**（4.7 段的 `AND rm.deleted_at
//!    IS NULL` 守卫；缺守卫会导致每次启动都 +1）。
//! 4. **`menu_seed_rendered_tree_per_role`** —— 端到端：登录后 `GET /iam/me` 返回的
//!    `data.menus` 树（前端真正消费的数据：`src/layouts/MainLayout.vue:109` 渲染
//!    `auth.menus`、`src/router/index.ts:486` 用 `meta.menuCode` 在这棵树里做成员判断）。
//!
//! ## 期望值的来源（防 tautology）
//!
//! 下面 5 份 `*_EXPECTED` 快照是**逐条读 `seeds/menu.sql` 第 4.1-4.5 段白名单推导**的，
//! 不是「跑一遍 DB 打印出来」的：
//! - 白名单里的 `settings_root` 被第 3.1 节 soft-delete，而 4.x 段的 INSERT 带
//!   `AND m.deleted_at IS NULL`，故它不入库（MANAGER 白名单 30 项 → 实际 29 项）；
//! - `floor_group` 只被第 3.2 段置 `is_active = false`（不软删），仍进授权表；
//! - 4.6 段 `DELETE` 会硬删 `settings_root` 等已软删菜单的 role_menu 行。
//!
//! ## 测试策略
//!
//! 与 `bootstrap_admin_seed.rs` 同形：不依赖 `src/main.rs` 启动钩子（避免并行测试间
//! 状态泄露），**测试内直接 `sqlx::raw_sql(include_str!(...))` 触发 seed**——与生产
//! 路径等价（同一份 SQL 文件 + 同样 raw_sql 执行），但时序可控。
//!
//! 注意 fresh database 是从 `hsh_erp_template` 克隆的，而 template 已 apply 过
//! `seeds/menu.sql`（见 `scripts/test_nextest.sh`），故**测试库开箱即处于收紧后的
//! 状态**：①③ 里的 seed 调用此时是 no-op（顺带验幂等），② 必须自己伪造 legacy 存量行。

use std::collections::{BTreeMap, BTreeSet};

use axum::http::StatusCode;
use chrono::NaiveDateTime;
use serde_json::Value;
use sqlx::{PgPool, Row};

use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token, send, test_app, test_pool, test_state,
};

/// 编译期嵌入 `seeds/menu.sql`（与生产启动钩子 `src/infra/seed.rs` 同一份文件）。
const MENU_SEED_SQL: &str = include_str!("../../seeds/menu.sql");

// ===========================================================================
// 常量：期望值（期望来自「读 seeds/menu.sql 第 4.1-4.5 段」，非跑库生成）
// ===========================================================================

/// 2026-10-05 收紧的 3 个目标 menuCode。
const TARGET_CODES: [&str; 3] = ["process_work_type", "part_process_chain", "worker_queue"];

/// `seeds/menu.sql` 第 4.1 段 MANAGER 白名单去掉 `settings_root`（第 3.1 节已 soft-delete）
/// 后的全量 live 授权 code 快照，**29 项**。
const MANAGER_EXPECTED: &[&str] = &[
    "applicants_list",
    "assemblies_list",
    "auth_group",
    "customer_management",
    "customers_list",
    "delivery_dispatch",
    "delivery_notes_manage",
    "floor_group",
    "home",
    "inspection_pending",
    "order_group",
    "outsource_companies_list",
    "outsource_list",
    "outsource_quotes_list",
    "outsource_send_receive_list",
    "part_process_chain",
    "parts_list",
    "parts_new",
    "pending_programming",
    "print_templates_designer",
    "process_work_type",
    "production_group",
    "production_stats",
    "repair_receive",
    "shelves_list",
    "template_management",
    "users_list",
    "worker_queue",
    "workers_list",
];

/// 第 4.2 段 CLERK 白名单，**16 项**（2026-10-05 已剔除 process_work_type /
/// part_process_chain；`worker_queue` 与 `production_group` 保留）。
const CLERK_EXPECTED: &[&str] = &[
    "applicants_list",
    "assemblies_list",
    "customer_management",
    "customers_list",
    "delivery_notes_manage",
    "home",
    "order_group",
    "outsource_companies_list",
    "outsource_list",
    "outsource_quotes_list",
    "outsource_send_receive_list",
    "parts_list",
    "parts_new",
    "production_group",
    "repair_receive",
    "worker_queue",
];

/// 第 4.3 段 INSPECTOR 白名单，**9 项**（2026-10-05 已剔除 3 个生产管理子菜单；
/// `production_group` / `inspection_pending` 保留）。
const INSPECTOR_EXPECTED: &[&str] = &[
    "assemblies_list",
    "delivery_dispatch",
    "delivery_notes_manage",
    "home",
    "inspection_pending",
    "outsource_send_receive_list",
    "parts_list",
    "production_group",
    "repair_receive",
];

/// 第 4.4 段 CNC_PROGRAMMER 白名单，**3 项**。
const CNC_PROGRAMMER_EXPECTED: &[&str] = &["home", "parts_list", "pending_programming"];

/// 第 4.5 段 SHELF_ACCOUNT 白名单，**3 项**。
const SHELF_ACCOUNT_EXPECTED: &[&str] = &["floor_group", "home", "scan_badge"];

/// 5 份快照 = 29 + 16 + 9 + 3 + 3 = **60 条 live 授权**。
const EXPECTED_SNAPSHOT: [(&str, &[&str]); 5] = [
    ("MANAGER", MANAGER_EXPECTED),
    ("CLERK", CLERK_EXPECTED),
    ("INSPECTOR", INSPECTOR_EXPECTED),
    ("CNC_PROGRAMMER", CNC_PROGRAMMER_EXPECTED),
    ("SHELF_ACCOUNT", SHELF_ACCOUNT_EXPECTED),
];

/// headline 口径：3 个目标 code 的逐角色可见集（用户需求原文的直接翻译）。
const HEADLINE: [(&str, &[&str]); 5] = [
    (
        "MANAGER",
        &["part_process_chain", "process_work_type", "worker_queue"],
    ),
    ("CLERK", &["worker_queue"]),
    ("INSPECTOR", &[]),
    ("CNC_PROGRAMMER", &[]),
    ("SHELF_ACCOUNT", &[]),
];

/// `seeds/menu.sql` 第 4.7 段回收的 5 对 `(role, menu_code)` —— 与 SQL 里的元组列表
/// 逐字对应。将来有人改 4.7 段，本常量会与之对不上（见
/// `menu_seed_revokes_legacy_grants`）。
const REVOKED_PAIRS: [(&str, &str); 5] = [
    ("CLERK", "process_work_type"),
    ("CLERK", "part_process_chain"),
    ("INSPECTOR", "process_work_type"),
    ("INSPECTOR", "part_process_chain"),
    ("INSPECTOR", "worker_queue"),
];

/// 伪造 legacy 授权行用的自定义 id 段（2026-10-05）。物理上与三方 ID 段都不相交：
/// - seed 静态段 `900000000100{0..4}xxx`（4.1-4.5 段的 `900000000100{role}001 + row_number()`）
/// - `t_menu` 静态段 `9000000000xxx`
/// - fixture 段 `9_000_000_000_000_000_1xx`（1e18 量级，雪花 ID 空间）
const LEGACY_GRANT_ID_BASE: i64 = 9_000_000_009_000_001;

/// ④ 用例给 `fx_iam_target` 插 INSPECTOR 角色的 `t_user_role.id`（同样避开
/// fixture 段 `9_000_000_000_000_000_115/116`）。
const INSPECTOR_USER_ROLE_ID: i64 = 9_000_000_009_000_101;

// ===========================================================================
// Helpers
// ===========================================================================

/// 跑一次 `seeds/menu.sql`（与生产启动钩子 `src/infra/seed.rs` 同一份 SQL）。
async fn apply_menu_seed(pool: &PgPool) {
    sqlx::raw_sql(MENU_SEED_SQL)
        .execute(pool)
        .await
        .expect("apply menu seed");
}

/// 每角色 live 授权的 code 集合（`rm.deleted_at IS NULL`，即前端可见性的授权侧输入）。
async fn live_codes_by_role(pool: &PgPool) -> BTreeMap<String, BTreeSet<String>> {
    let rows = sqlx::query(
        "SELECT rm.role, array_agg(m.code ORDER BY m.code) AS codes \
         FROM t_role_menu rm JOIN t_menu m ON m.id = rm.menu_id \
         WHERE rm.deleted_at IS NULL \
         GROUP BY rm.role ORDER BY rm.role",
    )
    .fetch_all(pool)
    .await
    .expect("查询 t_role_menu live 授权矩阵");

    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        let role: String = row.get("role");
        let codes: Vec<String> = row.get("codes");
        out.insert(role, codes.into_iter().collect());
    }
    out
}

/// 取某角色 code 集合；角色整体不在结果里（授权被清空）时返回空集 —— 由调用方的
/// 快照断言负责报出「整个角色都没了」。
fn codes_of(matrix: &BTreeMap<String, BTreeSet<String>>, role: &str) -> BTreeSet<String> {
    matrix.get(role).cloned().unwrap_or_default()
}

/// 该角色对 3 个目标 code 的可见集（与 `TARGET_CODES` 求交集）。
fn headline_of(matrix: &BTreeMap<String, BTreeSet<String>>, role: &str) -> BTreeSet<String> {
    codes_of(matrix, role)
        .into_iter()
        .filter(|c| TARGET_CODES.contains(&c.as_str()))
        .collect()
}

/// 断言 5 份全量快照（集合**完全相等**，不是「包含」）。
fn assert_full_snapshot(matrix: &BTreeMap<String, BTreeSet<String>>) {
    for (role, expected) in EXPECTED_SNAPSHOT {
        assert_code_set(role, &codes_of(matrix, role), expected);
    }
    // 兜底：不应冒出 4.1-4.5 段之外的角色（防止有人往 seed 里加第 6 个角色授权）
    let declared: BTreeSet<&str> = EXPECTED_SNAPSHOT.iter().map(|(r, _)| *r).collect();
    let actual: BTreeSet<&str> = matrix.keys().map(String::as_str).collect();
    assert_eq!(
        actual, declared,
        "t_role_menu 里出现了 seeds/menu.sql 第 4 节未声明的角色"
    );
}

/// 断言 3 个目标 code 的逐角色可见性（headline 口径）。
fn assert_headline(matrix: &BTreeMap<String, BTreeSet<String>>) {
    for (role, expected) in HEADLINE {
        let actual = headline_of(matrix, role);
        let expected_set: BTreeSet<String> = expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual, expected_set,
            "角色 {role} 对 3 个目标菜单（工序工种 / 制定工序 / 生产队列）的可见集不符 \
             （2026-10-05 收紧口径：工序工种+制定工序=仅 MANAGER，生产队列=MANAGER+CLERK，品检都看不到）"
        );
    }
}

/// 集合相等断言；失败信息直接列出「多了什么 / 少了什么」，避免整块 dump 让人自己找差异。
fn assert_code_set(role: &str, actual: &BTreeSet<String>, expected: &[&str]) {
    let expected: BTreeSet<String> = expected.iter().map(|s| s.to_string()).collect();
    if actual == &expected {
        return;
    }
    let extra: Vec<&str> = actual.difference(&expected).map(String::as_str).collect();
    let missing: Vec<&str> = expected.difference(actual).map(String::as_str).collect();
    panic!(
        "角色 {role} 的 live 授权 code 快照与 seeds/menu.sql 第 4 节白名单不符：\n\
         \x20 实际 {} 项 / 期望 {} 项\n\
         \x20 多出（白名单里没有，却仍在库里 = 该收回没收回）: {extra:?}\n\
         \x20 缺失（白名单里有，却不在库里）: {missing:?}\n\
         \x20 实际全集（排序）: {actual:?}",
        actual.len(),
        expected.len()
    );
}

/// 伪造一条 **live** `t_role_menu` 行：`menu_id` 按 code 反查（不写死 id，与 seed
/// 同一约定，避免环境间菜单 id 漂移）。模拟「今天的生产库：老 seed 留下的 live 授权行」。
async fn insert_live_grant(pool: &PgPool, id: i64, role: &str, code: &str, version: i32) {
    let affected = sqlx::query(
        "INSERT INTO t_role_menu \
           (id, role, menu_id, version, created_at, created_by, updated_at, updated_by, deleted_at) \
         SELECT $1, $2, m.id, $3, now(), 0, now(), 0, NULL \
         FROM t_menu m WHERE m.code = $4 AND m.deleted_at IS NULL",
    )
    .bind(id)
    .bind(role)
    .bind(version)
    .bind(code)
    .execute(pool)
    .await
    .expect("插入伪造 live t_role_menu 行");
    assert_eq!(
        affected.rows_affected(),
        1,
        "伪造 t_role_menu 行失败：code={code} 在 t_menu 里找不到 live 行（种子未 apply？）"
    );
}

/// 伪造一条**已软删**的 `t_role_menu` 行（模拟「上一版 seed 已回收过」），用来验 4.7 段
/// 的 `AND rm.deleted_at IS NULL` 守卫：缺守卫会导致每次重跑都再 bump 一次 version。
async fn insert_revoked_grant(pool: &PgPool, id: i64, role: &str, code: &str, version: i32) {
    let affected = sqlx::query(
        "INSERT INTO t_role_menu \
           (id, role, menu_id, version, created_at, created_by, updated_at, updated_by, deleted_at) \
         SELECT $1, $2, m.id, $3, now(), 0, now(), 0, now() \
         FROM t_menu m WHERE m.code = $4 AND m.deleted_at IS NULL",
    )
    .bind(id)
    .bind(role)
    .bind(version)
    .bind(code)
    .execute(pool)
    .await
    .expect("插入伪造已软删 t_role_menu 行");
    assert_eq!(
        affected.rows_affected(),
        1,
        "伪造 t_role_menu 行失败：code={code} 在 t_menu 里找不到 live 行（种子未 apply？）"
    );
}

/// 读回某条 `t_role_menu` 行的 `(deleted_at, version)`；行不存在直接 panic。
async fn read_grant(pool: &PgPool, id: i64) -> (Option<NaiveDateTime>, i32) {
    sqlx::query("SELECT deleted_at, version FROM t_role_menu WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .map(|row| {
            (
                row.get::<Option<NaiveDateTime>, _>("deleted_at"),
                row.get::<i32, _>("version"),
            )
        })
        .unwrap_or_else(|e| panic!("读 t_role_menu id={id} 失败：{e}"))
}

/// `t_role_menu` 总行数（live + 软删；回收只改 `deleted_at`、不增删行）。
async fn total_grant_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM t_role_menu")
        .fetch_one(pool)
        .await
        .expect("统计 t_role_menu 总行数")
}

/// 递归 DFS 收集渲染树里的全部 code。
fn collect_codes(nodes: &[Value], out: &mut BTreeSet<String>) {
    for node in nodes {
        if let Some(code) = node.get("code").and_then(Value::as_str) {
            out.insert(code.to_string());
        }
        if let Some(kids) = node.get("children").and_then(Value::as_array) {
            collect_codes(kids, out);
        }
    }
}

/// 在渲染树里按 code 深度优先找节点。
fn find_node<'a>(nodes: &'a [Value], code: &str) -> Option<&'a Value> {
    for node in nodes {
        if node.get("code").and_then(Value::as_str) == Some(code) {
            return Some(node);
        }
        if let Some(kids) = node.get("children").and_then(Value::as_array)
            && let Some(found) = find_node(kids, code)
        {
            return Some(found);
        }
    }
    None
}

/// 某节点的**直接**子节点 code 集合（结构断言用：专门防「父分组丢了导致子节点被
/// `build_menu_tree` 提升为顶级」的孤儿回归，见 `src/modules/iam/service/menu.rs:44-52`）。
fn children_of(tree: &[Value], code: &str) -> BTreeSet<String> {
    let node = find_node(tree, code).unwrap_or_else(|| panic!("渲染树里找不到节点 {code}"));
    node["children"]
        .as_array()
        .unwrap_or_else(|| panic!("节点 {code} 缺 children 数组"))
        .iter()
        .map(|c| {
            c.get("code")
                .and_then(Value::as_str)
                .unwrap_or("<无 code>")
                .to_string()
        })
        .collect()
}

/// 登录 + `GET /iam/me` → 返回 `data.menus` 原始树（前端直接消费的结构）。
async fn rendered_menus(app: &axum::Router, username: &str) -> Vec<Value> {
    let token = login_token(app, username, IamFixture::PASSWORD).await;
    let (status, env) = send(
        app.clone(),
        json_request("GET", "/iam/me", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "登录后 GET /iam/me 应 200，user={username} env={env}"
    );
    env["data"]["menus"]
        .as_array()
        .unwrap_or_else(|| panic!("/iam/me 响应缺 data.menus 数组，user={username} env={env}"))
        .clone()
}

// ===========================================================================
// 用例 ①：t_role_menu 授权口径的全量快照
// ===========================================================================
#[tokio::test]
async fn menu_seed_role_matrix() {
    let pool = test_pool().await;
    // fresh DB 继承自已 seed 的 template；这里仍显式跑一次走真实路径
    //（此时是 no-op，顺带把幂等也过了一遍）。
    apply_menu_seed(&pool).await;

    let matrix = live_codes_by_role(&pool).await;

    // ①-1 每角色 code 集合与「读 seeds/menu.sql 4.1-4.5 白名单」推导出的快照**完全相等**
    assert_full_snapshot(&matrix);

    // ①-2 headline：3 个目标 code 的逐角色可见性（失败信息能一眼看出哪个角色多了/少了哪个）
    assert_headline(&matrix);

    // ①-3 三方都要保留 production_group（CLERK 靠它挂 worker_queue、
    //     INSPECTOR 靠它挂 inspection_pending；丢了子节点会被提升为顶级）
    for role in ["MANAGER", "CLERK", "INSPECTOR"] {
        assert!(
            codes_of(&matrix, role).contains("production_group"),
            "角色 {role} 必须保留 production_group（生产管理分组）授权"
        );
    }
}

// ===========================================================================
// 用例 ②：回收（seeds/menu.sql 4.7 段）真的作用于存量行
// ===========================================================================
// 这是最关键的用例：4 节的授权写入是纯增量（`ON CONFLICT ... DO NOTHING`），
// **从白名单里删 code 不会回收生产库已存在的授权行**。若本用例缺席，将来有人删掉
// 4.7 段 / 把 code 加回白名单，用例 ①③④ 仍会全绿而线上菜单没收紧。
#[tokio::test]
async fn menu_seed_revokes_legacy_grants() {
    let pool = test_pool().await;

    // 1. 先跑一次 seed，把 fresh DB 置于收紧后状态
    apply_menu_seed(&pool).await;

    // 2. 手工伪造 legacy 存量：5 条 **live**（deleted_at 留空）授权行，模拟「今天的
    //    生产库 —— 老版本 seed 留下的、尚未被 4.7 段回收的授权行」。
    //    （测试库是从已 seed 的 template 克隆的，这 5 对在库里本来一行都没有，
    //    所以必须自己造，否则回收无从发生、断言就成了空转。）
    for (idx, (role, code)) in REVOKED_PAIRS.iter().enumerate() {
        let id = LEGACY_GRANT_ID_BASE + idx as i64;
        insert_live_grant(&pool, id, role, code, 0).await;

        // 前提校验：这 5 行确实必须是 live 的
        let (deleted_at, _) = read_grant(&pool, id).await;
        assert_eq!(
            deleted_at, None,
            "伪造的 legacy 授权行 id={id} ({role}/{code}) 必须是 live（deleted_at IS NULL），\
             否则本用例的前提不成立"
        );
    }

    // 3. 记下回收前的总行数（live + 软删）
    let rows_before = total_grant_rows(&pool).await;

    // 4. 再跑一次 seed —— 4.7 段应当把这 5 行软删
    apply_menu_seed(&pool).await;

    // 5. 断言这 5 行变成 `deleted_at IS NOT NULL`（**软删**、不是物理消失，留审计）
    for (idx, (role, code)) in REVOKED_PAIRS.iter().enumerate() {
        let id = LEGACY_GRANT_ID_BASE + idx as i64;
        let (deleted_at, version) = read_grant(&pool, id).await;
        assert!(
            deleted_at.is_some(),
            "4.7 段必须把存量授权行 {role}/{code}（id={id}）**软删**（deleted_at IS NOT NULL）；\
             实际仍为 live —— 说明回收段没生效（或白名单被加回了这个 code）"
        );
        assert_eq!(
            version, 1,
            "回收应恰好 bump 一次 version（0 → 1），id={id} ({role}/{code})"
        );
    }

    // 6. 断言 4.7 段只改 `deleted_at`、**不增删行**
    let rows_after = total_grant_rows(&pool).await;
    assert_eq!(
        rows_after, rows_before,
        "回收只允许改 deleted_at，不允许增删 t_role_menu 行（回收前 {rows_before} / 回收后 {rows_after}）"
    );

    // 7. 断言「可见矩阵」回到收紧后状态（即用例 ① 的 headline 成立）
    let matrix = live_codes_by_role(&pool).await;
    assert_headline(&matrix);
    assert_full_snapshot(&matrix);
}

// ===========================================================================
// 用例 ③：幂等（重跑 3 次）
// ===========================================================================
#[tokio::test]
async fn menu_seed_idempotent() {
    let pool = test_pool().await;

    // ── 第 1 次 ──
    apply_menu_seed(&pool).await;
    let snap1 = live_codes_by_role(&pool).await;
    assert_full_snapshot(&snap1);

    // 伪造「上一版 seed 已回收」的 5 条软删行（version=0），用来验 4.7 段的
    // `AND rm.deleted_at IS NULL` 守卫：**缺守卫会导致每次重跑都 +1 version**。
    for (idx, (role, code)) in REVOKED_PAIRS.iter().enumerate() {
        insert_revoked_grant(&pool, LEGACY_GRANT_ID_BASE + idx as i64, role, code, 0).await;
    }
    let before: Vec<(Option<NaiveDateTime>, i32)> = read_revoked_rows(&pool).await;
    assert!(
        before
            .iter()
            .all(|(deleted_at, version)| deleted_at.is_some() && *version == 0),
        "伪造的 5 条已回收行应处于 (soft_deleted, version=0) 状态，实际 {before:?}"
    );

    // ── 第 2 次 ──
    apply_menu_seed(&pool).await;
    let snap2 = live_codes_by_role(&pool).await;
    let mid: Vec<(Option<NaiveDateTime>, i32)> = read_revoked_rows(&pool).await;

    // ── 第 3 次 ──
    apply_menu_seed(&pool).await;
    let snap3 = live_codes_by_role(&pool).await;
    let after: Vec<(Option<NaiveDateTime>, i32)> = read_revoked_rows(&pool).await;

    // ③-1 无重复 live 行（`uk_t_role_menu_role_menu` 是 partial unique index，
    //     正常 insert 会被 DB 挡；这里防的是「有人改成 upsert 覆盖」这类回归）
    let dupes: Vec<(String, i64, i64)> = sqlx::query(
        "SELECT rm.role, rm.menu_id, count(*) AS n \
         FROM t_role_menu rm WHERE rm.deleted_at IS NULL \
         GROUP BY rm.role, rm.menu_id HAVING count(*) > 1 ORDER BY rm.role, rm.menu_id",
    )
    .fetch_all(&pool)
    .await
    .expect("查重复 live 授权行")
    .into_iter()
    .map(|r| (r.get("role"), r.get("menu_id"), r.get("n")))
    .collect();
    assert!(
        dupes.is_empty(),
        "t_role_menu 不应存在重复 live 行（role, menu_id），实际 {dupes:?}"
    );

    // ③-2 每角色 live code 集合在 3 次之间**逐次完全相同**
    assert_eq!(
        snap2, snap1,
        "第 2 次跑 seed 改变了可见授权矩阵（幂等被破坏）"
    );
    assert_eq!(
        snap3, snap2,
        "第 3 次跑 seed 改变了可见授权矩阵（幂等被破坏）"
    );
    assert_full_snapshot(&snap3);

    // ③-3 那 5 条已回收的软删行，第 2/3 次跑之后 `(deleted_at, version)` 都没再变
    //     （4.7 段的 `AND rm.deleted_at IS NULL` 守卫生效）
    assert_eq!(
        mid, before,
        "第 2 次跑 seed 改动了已回收的软删行（4.7 段缺 `AND rm.deleted_at IS NULL` 守卫？）"
    );
    assert_eq!(
        after, mid,
        "第 3 次跑 seed 改动了已回收的软删行（4.7 段缺 `AND rm.deleted_at IS NULL` 守卫？）"
    );
}

/// 读回 `REVOKED_PAIRS` 对应 5 条伪造行的 `(deleted_at, version)`（按 id 序）。
async fn read_revoked_rows(pool: &PgPool) -> Vec<(Option<NaiveDateTime>, i32)> {
    let mut out = Vec::with_capacity(REVOKED_PAIRS.len());
    for idx in 0..REVOKED_PAIRS.len() {
        out.push(read_grant(pool, LEGACY_GRANT_ID_BASE + idx as i64).await);
    }
    out
}

// ===========================================================================
// 用例 ④：端到端 —— 登录后 `/iam/me` 返回的菜单树
// ===========================================================================
// 这是前端真正消费的数据（`frontend/src/layouts/MainLayout.vue:109` 渲染
// `auth.menus`、`frontend/src/router/index.ts:486` 用 `meta.menuCode` 在这棵树里做成员判断）。
#[tokio::test]
async fn menu_seed_rendered_tree_per_role() {
    let pool = test_pool().await;
    let _fx = load_iam_fixture(&pool).await;
    // fixture 里没有 INSPECTOR 用户；给无角色的 `fx_iam_target` 插一条全局 INSPECTOR
    // 角色（scope_type / scope_id 留空 = 非货架账号）。
    add_inspector_role(&pool).await;

    apply_menu_seed(&pool).await;
    let app = test_app(test_state(pool.clone()).await);

    let mgr_tree = rendered_menus(&app, IamFixture::MANAGER_USERNAME).await;
    let clerk_tree = rendered_menus(&app, IamFixture::CLERK_USERNAME).await;
    let insp_tree = rendered_menus(&app, IamFixture::TARGET_USERNAME).await;

    let mut mgr = BTreeSet::new();
    collect_codes(&mgr_tree, &mut mgr);
    let mut clerk = BTreeSet::new();
    collect_codes(&clerk_tree, &mut clerk);
    let mut insp = BTreeSet::new();
    collect_codes(&insp_tree, &mut insp);

    // ── sanity：三方都必须能看到 home（证明树非空、DFS 收集方法本身有效）──
    for (role, codes) in [("MANAGER", &mgr), ("CLERK", &clerk), ("INSPECTOR", &insp)] {
        assert!(
            codes.contains("home"),
            "角色 {role} 的渲染树里应能看到 home（sanity：证明树非空 + DFS 有效）"
        );
    }

    // ── MANAGER：三个目标 code 全在 ──
    for code in TARGET_CODES {
        assert!(
            mgr.contains(code),
            "MANAGER 渲染树里应包含 {code}（工序工种 / 制定工序 / 生产队列 都需要 MANAGER）"
        );
    }
    assert!(
        mgr.contains("production_group"),
        "MANAGER 渲染树里应包含 production_group"
    );
    assert_eq!(
        children_of(&mgr_tree, "production_group"),
        BTreeSet::from([
            "inspection_pending".to_string(),
            "part_process_chain".to_string(),
            "pending_programming".to_string(),
            "process_work_type".to_string(),
            "worker_queue".to_string(),
            "workers_list".to_string(),
        ]),
        "MANAGER 的 production_group 子节点集合不符（6 个：工序工种/制定工序/待编程/生产队列/工人一览/待品检）"
    );

    // ── CLERK：只有 worker_queue 在，另两个收回了 ──
    assert!(
        clerk.contains("worker_queue"),
        "CLERK 渲染树里应包含 worker_queue（生产队列需要 CLERK）"
    );
    for code in ["process_work_type", "part_process_chain"] {
        assert!(
            !clerk.contains(code),
            "CLERK 渲染树里不应包含 {code}（2026-10-05 收回，改 MANAGER 专有）"
        );
    }
    // 结构断言：worker_queue 必须**挂在** production_group 下，而不是被提升为顶级
    assert_eq!(
        children_of(&clerk_tree, "production_group"),
        BTreeSet::from(["worker_queue".to_string()]),
        "CLERK 的 production_group 下应只剩 worker_queue 一个子节点"
    );

    // ── INSPECTOR：三个目标 code 都不在，但 production_group / inspection_pending 都在 ──
    for code in TARGET_CODES {
        assert!(
            !insp.contains(code),
            "INSPECTOR 渲染树里不应包含 {code}（品检看不到生产管理的这 3 个菜单）"
        );
    }
    assert!(
        insp.contains("production_group"),
        "INSPECTOR 渲染树里应包含 production_group（其 inspection_pending 子菜单要挂上去）"
    );
    assert!(
        insp.contains("inspection_pending"),
        "INSPECTOR 渲染树里应包含 inspection_pending（待品检）"
    );
    // 结构断言：inspection_pending 必须挂在 production_group 下 —— 这条专门防「父分组
    // 丢了导致子节点被 build_menu_tree 提升为顶级」的孤儿回归
    //（见 `src/modules/iam/service/menu.rs:44-52` 的孤儿兜底分支）。
    assert_eq!(
        children_of(&insp_tree, "production_group"),
        BTreeSet::from(["inspection_pending".to_string()]),
        "INSPECTOR 的 production_group 下应只剩 inspection_pending 一个子节点 \
         （若为空说明 inspection_pending 被提升成了顶级 → 父分组授权丢了）"
    );

    // ── 渲染口径 ≠ 授权口径 ──
    // `assemblies_list` 在三方的 t_role_menu 里都是 live 授权，但 `t_menu.is_active=false`
    // （seed 3.4 段），所以渲染树里三方都**不该**出现它
    //（过滤见 `src/modules/iam/repo/sql/menu.rs:8-31` 的 `m.is_active = TRUE`）。
    let matrix = live_codes_by_role(&pool).await;
    for role in ["MANAGER", "CLERK", "INSPECTOR"] {
        assert!(
            codes_of(&matrix, role).contains("assemblies_list"),
            "前提校验失败：{role} 的 t_role_menu 里本应有 assemblies_list 的 live 授权"
        );
    }
    for (role, codes) in [("MANAGER", &mgr), ("CLERK", &clerk), ("INSPECTOR", &insp)] {
        assert!(
            !codes.contains("assemblies_list"),
            "角色 {role} 的渲染树里不应出现 assemblies_list（t_menu.is_active=false，被渲染层过滤）"
        );
    }
}

/// 给无角色的 `fx_iam_target` 插一条全局 INSPECTOR 角色。
async fn add_inspector_role(pool: &PgPool) {
    let affected = sqlx::query(
        "INSERT INTO t_user_role \
           (id, user_id, role, scope_type, scope_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'INSPECTOR', NULL, NULL, 0, now(), 0, now(), 0)",
    )
    .bind(INSPECTOR_USER_ROLE_ID)
    .bind(IamFixture::TARGET_USER_ID)
    .execute(pool)
    .await
    .expect("给 fx_iam_target 插 INSPECTOR 角色");
    assert_eq!(
        affected.rows_affected(),
        1,
        "给 fx_iam_target 插 INSPECTOR 角色应命中 1 行"
    );
}
