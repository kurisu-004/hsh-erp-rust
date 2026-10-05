//! prod::process_design 域端到端集成测试（2026-10-05 新增）
//!
//! 端点：`GET /api/v2/prod/process-design/parts`（测试内路径 `/prod/process-design/parts`）
//!
//! 覆盖 8 个场景：
//!   1. **子件可见（★ 本次核心回归锁）**：`assembly_id` 指向 `t_assembly` 的 PENDING
//!      子零件必须出现在 `items` 里，且该行 `assembly_id` 有值、`process_chain_id` 为
//!      null —— 锁死「不得加回 `AND assembly_id IS NULL` 守卫」
//!   2. 软删闸门：`deleted_at` 非空的 PENDING 行不出现
//!   3. 状态闸门：`IN_PROCESS` / `PROGRAMMING` / `COMPLETED` / `CANCELLED` 全部不出现
//!   4. `total` 与 `items` 口径一致：`total` 是全量数而非本页数（造行数 > limit）
//!   5. clamp 边界：`limit=0 → 1`；`limit=99999 → 500`；`offset=-5 → 0`（断言回显）
//!   6. 排序：`sort_dir=DESC` 真倒序；`sort_dir=garbage` 退化为 `ASC`
//!   7. `serial_no` 为 null 的行排在末尾（ASC 与 DESC **两种方向各断言一次** ——
//!      `NULLS LAST` 是显式写死的，两种方向都该在末尾）
//!   8. 角色守卫：MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER 四角色各自放行 +
//!      SHELF_ACCOUNT 被拒（HTTP 403 + 错误码 40300）
//!
//! ## 串行化
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex / `--test-threads=1` 双保险。每个测试内部造自己的差异行，
//! 互不污染；断言一律具体到「查到哪几个 id」。
//!
//! ## 集成测试范本（PR13 Phase F / H）
//! HTTP helper（`send` / `json_request` / `login_token` / `test_app` / `test_state` /
//! `test_pool`）与 fixture 全部走 `hsh_erp_test_support`，本文件**不**重复声明本地
//! helper；只有「按场景造差异行」这一个动作留作本地 `async fn`（与
//! `pending_programming.rs` 同惯例 —— 域内独享的 raw SQL 构造保留为本地 fn）。

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_test_support::{
    ProcessDesignFixture, json_request, load_process_design_fixture, login_token, send, test_app,
    test_pool, test_state,
};

/// 制定工序零件列表端点路径（测试 app 不带 `/api/v2` 前缀）。
const PARTS_URI: &str = "/prod/process-design/parts";

/// fixture 预置的、**应当出现**在列表里的 PENDING 零件 id（场景 1 / 4 的全量口径基线）。
///
/// 顺序按 fixture 的 serial_no 字典序（`NULLS LAST`）：
/// PD-ASM / PD-F1001-01 / PD-F1001-02 / PD-F1001-03 / PD-F1001-04 / (no-serial)
const EXPECTED_IDS: [i64; 6] = [
    ProcessDesignFixture::PART_ASSEMBLY,
    ProcessDesignFixture::PART_BASE_1,
    ProcessDesignFixture::PART_BASE_2,
    ProcessDesignFixture::PART_CHAINED,
    ProcessDesignFixture::PART_CHILD,
    ProcessDesignFixture::PART_NO_SERIAL,
];

// ===========================================================================
//  Bootstrap
// ===========================================================================

/// fresh database + process_design fixture（提供 5 个角色账号）→ 以 MANAGER 身份登录。
async fn bootstrap() -> (PgPool, axum::Router, String, ProcessDesignFixture) {
    let pool = test_pool().await;
    let fx = load_process_design_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, ProcessDesignFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  本域独享 raw SQL helper（造场景差异行）
// ===========================================================================

/// 插一行 PENDING `t_part`（`serial_no` 可空 → 验 NULLS LAST 的前提）。
///
/// `serial_no` 传 `None` 即走 SQL NULL（手工工单形态）。
async fn insert_pending_part(
    pool: &PgPool,
    customer_id: i64,
    serial_no: Option<&str>,
    name: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = hsh_erp_test_support::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, status, is_urgent, customer_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-PD-DYN', 'PD', 1, $4, $4, 'PENDING', false, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(now.date())
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part (PENDING)");
    id
}

// ===========================================================================
//  断言 helper
// ===========================================================================

/// 打一次列表端点，断言 200 + `code == 0`，返回信封。
async fn get_parts(app: &axum::Router, token: &str, query: &str) -> Value {
    let uri = if query.is_empty() {
        PARTS_URI.to_string()
    } else {
        format!("{PARTS_URI}?{query}")
    };
    let (status, env) = send(app.clone(), json_request("GET", &uri, None, Some(token))).await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(env["code"], 0, "GET {uri}: {env}");
    env
}

/// 从信封里抽出按返回顺序排好的 item id 列表（雪花 i64 → string）。
fn item_ids(env: &Value) -> Vec<String> {
    env["data"]["items"]
        .as_array()
        .expect("data.items")
        .iter()
        .map(|it| {
            it["id"]
                .as_str()
                .expect("item.id 必须是 string(i64)")
                .to_string()
        })
        .collect()
}

/// 把 id 列表排序（消除 ORDER BY 无关的行间不确定性，便于精确集合断言）。
fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// 从信封里按 id 取单条 item（找不到则 panic，打印整份信封便于定位）。
fn item_by_id(env: &Value, id: i64) -> &Value {
    let want = id.to_string();
    env["data"]["items"]
        .as_array()
        .expect("data.items")
        .iter()
        .find(|it| it["id"].as_str() == Some(want.as_str()))
        .unwrap_or_else(|| panic!("id {id} 不在结果里: {env}"))
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1（★ 核心回归）: 装配件子零件必须可见。
///
/// 这是本端点存在的全部理由。part 域 `GET /parts` 在 service 层硬置
/// `part_only: true`，repo 据此在 SQL 里加 `AND assembly_id IS NULL`，把子件全部
/// 排除（`t_part.assembly_id` 是子件指向父装配件的逻辑 FK）→ 装配件的子零件在
/// 「制定工序」页看不到。本端点**刻意不加**该守卫，故子件必须正常返回。
///
/// 断言三层：
/// 1. 子件出现在 `items` 里；
/// 2. 该行 `assembly_id` 是 JSON string 形态且等于 `t_assembly.id`；
/// 3. 该行 `process_chain_id` 为 `null`（未制定工序 = 本页的主闸门标记）。
#[tokio::test]
async fn assembly_child_parts_are_visible() {
    let (pool, app, token, _fx) = bootstrap().await;

    // 前提：fixture 的子件行确实是「PENDING + assembly_id 指向装配件 + 未挂工艺链」
    let (status, assembly_id, chain_id): (String, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT status, assembly_id, process_chain_id FROM t_part WHERE id = $1",
        )
        .bind(ProcessDesignFixture::PART_CHILD)
        .fetch_one(&pool)
        .await
        .expect("select 子件行");
    assert_eq!(status, "PENDING", "前提：子件是 PENDING");
    assert_eq!(
        assembly_id,
        Some(ProcessDesignFixture::ASSEMBLY_ID),
        "前提：子件行 assembly_id 指向 t_assembly"
    );
    assert_eq!(chain_id, None, "前提：子件尚未制定工序");

    let env = get_parts(&app, &token, "").await;

    // ① 子件在列表里
    let ids = item_ids(&env);
    assert!(
        ids.contains(&ProcessDesignFixture::PART_CHILD.to_string()),
        "装配件子件 {} 必须出现在列表里（不得被 AND assembly_id IS NULL 排除）: {env}",
        ProcessDesignFixture::PART_CHILD
    );

    // ② assembly_id 有值（JSON string 形态的雪花 id）
    let child = item_by_id(&env, ProcessDesignFixture::PART_CHILD);
    assert_eq!(
        child["assembly_id"].as_str(),
        Some(ProcessDesignFixture::ASSEMBLY_ID.to_string().as_str()),
        "子件行 assembly_id 必须等于父装配件 id（string(i64)）: {env}"
    );

    // ③ process_chain_id 为 null（未制定工序）
    assert!(
        child["process_chain_id"].is_null(),
        "子件尚未制定工序 → process_chain_id 应为 null: {env}"
    );

    // 反向确认：独立零件（assembly_id IS NULL）的行该字段仍是 null，
    // 说明「子件可见」不是「所有行都被塞了 assembly_id」造成的假象
    let base = item_by_id(&env, ProcessDesignFixture::PART_BASE_1);
    assert!(
        base["assembly_id"].is_null(),
        "独立零件 assembly_id 应为 null: {env}"
    );

    // 已挂工艺链的行：process_chain_id 非 null（验该字段真的会取到值，
    // 否则场景 1 的「为 null」断言会因「恒为 null」而空过）
    let chained = item_by_id(&env, ProcessDesignFixture::PART_CHAINED);
    assert_eq!(
        chained["process_chain_id"].as_str(),
        Some(ProcessDesignFixture::CHAIN_ID.to_string().as_str()),
        "已挂链零件的 process_chain_id 必须取到真值: {env}"
    );

    // 前置反例：同一行若走 part 域 GET /parts 的 part_only 守卫就会被排除 —— 这里
    // 显式确认 part 域旧端点**确实**看不到子件，把「两者口径差异」钉成事实，
    // 避免后人误以为本端点与旧端点等价。
    let (pstatus, penv) = send(
        app.clone(),
        json_request(
            "GET",
            "/parts?status=PENDING&limit=200",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(pstatus, StatusCode::OK, "GET /parts: {penv}");
    let part_ids = penv["data"]["items"]
        .as_array()
        .expect("part 域 data.items")
        .iter()
        .map(|it| it["id"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert!(
        !part_ids.contains(&ProcessDesignFixture::PART_CHILD.to_string()),
        "前提失效：part 域 GET /parts 竟返回了子件（part_only 守卫不再生效？）: {penv}"
    );
}

/// 场景 2: 软删闸门 —— `deleted_at` 非空的 PENDING 行不出现。
#[tokio::test]
async fn soft_deleted_pending_parts_excluded() {
    let (_pool, app, token, _fx) = bootstrap().await;

    // 前提：fixture 软删行确实是「PENDING + deleted_at 非空」
    let (status, deleted_at): (String, Option<chrono::NaiveDateTime>) =
        sqlx::query_as("SELECT status, deleted_at FROM t_part WHERE id = $1")
            .bind(ProcessDesignFixture::PART_SOFT_DELETED)
            .fetch_one(&_pool)
            .await
            .expect("select 软删行");
    assert_eq!(status, "PENDING", "前提：软删行状态是 PENDING");
    assert!(deleted_at.is_some(), "前提：软删行 deleted_at 非空");

    let env = get_parts(&app, &token, "").await;
    let ids = item_ids(&env);
    assert!(
        !ids.contains(&ProcessDesignFixture::PART_SOFT_DELETED.to_string()),
        "软删行 {} 不该出现（deleted_at IS NULL 闸门）: {env}",
        ProcessDesignFixture::PART_SOFT_DELETED
    );
    assert_eq!(
        sorted(ids),
        sorted(EXPECTED_IDS.iter().map(|i| i.to_string()).collect()),
        "除软删行外应恰好剩 fixture 的 6 行: {env}"
    );
}

/// 场景 3: 状态闸门 —— 四种非 PENDING 状态各一行，全部不出现。
#[tokio::test]
async fn non_pending_statuses_excluded() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let env = get_parts(&app, &token, "").await;
    let ids = item_ids(&env);
    for (label, id) in [
        ("IN_PROCESS", ProcessDesignFixture::PART_IN_PROCESS),
        ("PROGRAMMING", ProcessDesignFixture::PART_PROGRAMMING),
        ("COMPLETED", ProcessDesignFixture::PART_COMPLETED),
        ("CANCELLED", ProcessDesignFixture::PART_CANCELLED),
    ] {
        assert!(
            !ids.contains(&id.to_string()),
            "{label} 状态的零件 {id} 不该出现（PENDING 是本页业务闸门）: {env}"
        );
    }
    assert_eq!(
        sorted(ids),
        sorted(EXPECTED_IDS.iter().map(|i| i.to_string()).collect()),
        "只该剩 PENDING 的 6 行: {env}"
    );
}

/// 场景 4: `total` 是全量数而非本页数（造若干行 + 超 limit）。
#[tokio::test]
async fn total_is_full_count_not_page_count() {
    let (pool, app, token, fx) = bootstrap().await;

    // fixture 6 行 + 本例追加 3 行 = 9 行；用 limit=4 翻页
    for i in 0..3 {
        insert_pending_part(&pool, fx.customer_id, Some(&format!("PD-EXTRA-{i:02}")), "extra")
            .await;
    }

    let env = get_parts(&app, &token, "limit=4").await;
    assert_eq!(env["data"]["total"], 9, "total 应是全量 9 行: {env}");
    assert_eq!(
        env["data"]["items"]
            .as_array()
            .expect("items")
            .len(),
        4,
        "本页只该返 4 条（limit=4）: {env}"
    );

    // 翻页后 total 不变（口径与 offset 无关）
    let page2 = get_parts(&app, &token, "limit=4&offset=4").await;
    assert_eq!(page2["data"]["total"], 9, "翻页后 total 仍应是 9: {page2}");
    assert_eq!(
        page2["data"]["offset"], 4,
        "offset 回显: {page2}"
    );

    // 不带 limit 时默认 200 → 全量返回，items 数 == total
    let all = get_parts(&app, &token, "").await;
    assert_eq!(all["data"]["limit"], 200, "缺省 limit 应为 200: {all}");
    assert_eq!(
        all["data"]["items"].as_array().expect("items").len(),
        9,
        "缺省 limit=200 应一次返回全部 9 行: {all}"
    );
    assert_eq!(
        all["data"]["total"],
        all["data"]["items"].as_array().expect("items").len() as i64,
        "未超 limit 时 items 数必须等于 total: {all}"
    );
}

/// 场景 5: clamp 边界 —— `limit=0 → 1`；`limit=99999 → 500`；`offset=-5 → 0`。
///
/// 断言响应回显的 `limit` / `offset`（不必真造 500 行 —— 回显值才是 clamp 的契约）。
#[tokio::test]
async fn limit_offset_clamped() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let env = get_parts(&app, &token, "limit=0").await;
    assert_eq!(env["data"]["limit"], 1, "limit=0 → clamp 1: {env}");
    assert_eq!(
        env["data"]["items"].as_array().expect("items").len(),
        1,
        "limit=0 时本页只该返 1 条: {env}"
    );

    let env = get_parts(&app, &token, "limit=99999").await;
    assert_eq!(env["data"]["limit"], 500, "limit=99999 → clamp 500: {env}");

    let env = get_parts(&app, &token, "offset=-5").await;
    assert_eq!(env["data"]["offset"], 0, "offset=-5 → max 0: {env}");
    assert_eq!(
        env["data"]["items"]
            .as_array()
            .expect("items")
            .len(),
        EXPECTED_IDS.len(),
        "offset 归 0 → 返回全部 6 条: {env}"
    );

    // 边界值本身：limit=1 / limit=500 不被 clamp
    let env = get_parts(&app, &token, "limit=1").await;
    assert_eq!(env["data"]["limit"], 1, "limit=1 恰好在边界内: {env}");
    let env = get_parts(&app, &token, "limit=500").await;
    assert_eq!(env["data"]["limit"], 500, "limit=500 恰好在边界内: {env}");

    // 空串 / 全空白走缺省（URL query 无类型之分，裸 `?limit=` 不该 400）
    let env = get_parts(&app, &token, "limit=&offset=").await;
    assert_eq!(env["data"]["limit"], 200, "空串 limit → 缺省 200: {env}");
    assert_eq!(env["data"]["offset"], 0, "空串 offset → 缺省 0: {env}");
}

/// 场景 6: 排序 —— `DESC` 真倒序；`garbage` 退化为 `ASC`。
#[tokio::test]
async fn sort_dir_desc_and_invalid_falls_back_to_asc() {
    let (_pool, app, token, _fx) = bootstrap().await;

    // 造 3 行连续序列号（字典序与数值序同向，故断言可直读）
    insert_pending_part(&_pool, _fx.customer_id, Some("F1001-01"), "sort 1").await;
    insert_pending_part(&_pool, _fx.customer_id, Some("F1001-02"), "sort 2").await;
    insert_pending_part(&_pool, _fx.customer_id, Some("F1001-03"), "sort 3").await;

    // fixture 预置行（PD-*）与本例造的 F1001-* 混在同一结果集里，故只提取
    // F1001-01/02/03 这三行的相对次序断言，完整集合断言交给 EXPECTED_IDS 那几处场景。
    let ascending = vec!["F1001-01", "F1001-02", "F1001-03"];

    /// 抽出 serial_no ∈ {F1001-01,02,03} 的子序列（保持返回顺序）。
    fn rel(env: &Value) -> Vec<String> {
        let want = ["F1001-01", "F1001-02", "F1001-03"];
        env["data"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|it| it["serial_no"].as_str())
            .filter(|sn| want.contains(sn))
            .map(str::to_string)
            .collect()
    }

    let env = get_parts(&app, &token, "").await;
    assert_eq!(
        rel(&env),
        ascending,
        "缺省 sort_dir 应为 ASC（F1001-01 → 02 → 03）: {env}"
    );

    let desc = get_parts(&app, &token, "sort_dir=DESC").await;
    let mut reversed = ascending.clone();
    reversed.reverse();
    assert_eq!(rel(&desc), reversed, "sort_dir=DESC 应真倒序: {desc}");

    // 小写也认（大小写不敏感）
    let lower = get_parts(&app, &token, "sort_dir=desc").await;
    assert_eq!(rel(&lower), reversed, "sort_dir=desc（小写）应同样生效: {lower}");

    // 非法值退化为 ASC，不报错
    let garbage = get_parts(&app, &token, "sort_dir=garbage").await;
    assert_eq!(
        rel(&garbage),
        ascending,
        "sort_dir=garbage 应退化为 ASC（不报错）: {garbage}"
    );

    // ⚠️ 字典序不是数值序（口径确认，**不是缺陷**）。`serial_no` 是 varchar，
    // 按字符比较：第 7 位上 '1' < '2'，故 `F1001-10` 排在 `F1001-2` **之前**，
    // 尽管数值上 10 > 2。这与 part 域旧端点 `sort_by=SERIAL_NO` 的排序同构
    // （同一列、同一 collation），前端切端点后排序观感不变 —— 故按现状保留。
    let s10 = insert_pending_part(&_pool, _fx.customer_id, Some("F1001-10"), "sort 10").await;
    let s2 = insert_pending_part(&_pool, _fx.customer_id, Some("F1001-2"), "sort 2short").await;
    let env = get_parts(&app, &token, "").await;
    let ids = item_ids(&env);
    let ten_pos = ids
        .iter()
        .position(|id| id == &s10.to_string())
        .expect("F1001-10 行必须在结果里");
    let two_pos = ids
        .iter()
        .position(|id| id == &s2.to_string())
        .expect("F1001-2 行必须在结果里");
    assert!(
        ten_pos < two_pos,
        "serial_no 是 varchar → 字典序（F1001-10 排在 F1001-2 之前，数值上恰相反）是\
         **既定口径**，与 part 域旧端点 sort_by=SERIAL_NO 同构，不是 bug: {env}"
    );
}

/// 场景 7: `serial_no` 为 null 的行排在末尾（ASC 与 DESC 各断言一次）。
///
/// `NULLS LAST` 在 SQL 里是**显式写死**的 —— PG 的 `DESC` 默认其实是
/// `NULLS FIRST`，故 DESC 方向单独验一遍：若哪天有人把显式子句删掉，本用例会红。
#[tokio::test]
async fn null_serial_no_sorted_last_in_both_directions() {
    let (_pool, app, token, _fx) = bootstrap().await;

    for (dir, query) in [("ASC", ""), ("DESC", "sort_dir=DESC")] {
        let env = get_parts(&app, &token, query).await;
        let ids = item_ids(&env);
        let null_pos = ids
            .iter()
            .position(|id| id == &ProcessDesignFixture::PART_NO_SERIAL.to_string())
            .unwrap_or_else(|| panic!("[{dir}] 无序列号行必须出现在结果里: {env}"));
        assert_eq!(
            null_pos,
            ids.len() - 1,
            "[{dir}] serial_no 为 null 的行必须排在**末尾**（NULLS LAST）: {env}"
        );
        // 该行的 serial_no 字段本身必须是 JSON null（不是空串）
        let item = item_by_id(&env, ProcessDesignFixture::PART_NO_SERIAL);
        assert!(
            item["serial_no"].is_null(),
            "[{dir}] 手工工单的 serial_no 应序列化为 null（不是 \"\"）: {env}"
        );
    }
}

/// 场景 8: 角色守卫 —— 四角色各自放行；SHELF_ACCOUNT → 403 + 40300。
#[tokio::test]
async fn role_guard_allows_four_roles_rejects_shelf_account() {
    let (_pool, app, _token, fx) = bootstrap().await;

    for (label, username) in [
        ("MANAGER", fx.manager_username.as_str()),
        ("CLERK", fx.clerk_username.as_str()),
        ("INSPECTOR", fx.inspector_username.as_str()),
        ("CNC_PROGRAMMER", fx.cnc_username.as_str()),
    ] {
        let token = login_token(&app, username, ProcessDesignFixture::PASSWORD).await;
        let (status, env) = send(
            app.clone(),
            json_request("GET", PARTS_URI, None, Some(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label} 应可访问: {env}");
        assert_eq!(env["code"], 0, "{label}: {env}");
    }

    // SHELF_ACCOUNT —— 合法登录（scope 到检验架）但越权
    let shelf_token = login_token(&app, &fx.shelf_username, ProcessDesignFixture::PASSWORD).await;
    let (status, env) = send(
        app,
        json_request("GET", PARTS_URI, None, Some(&shelf_token)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "SHELF_ACCOUNT 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN 错误码: {env}");
}
