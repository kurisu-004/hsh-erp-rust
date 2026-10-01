//! prod::shelf_process 子模块端到端集成测试（2026-10-02 自 tests/shelf/api.rs 迁入）
//!
//! ## 覆盖
//! 1. `set_shelf_processes_replaces_existing_mapping` —— 整组替换：先 set [P1]，
//!    再 set [P2, P3] 后旧映射 (P1) 软删、新映射 (P2+P3) 在场。
//!    （原 `tests/shelf/api.rs::set_shelf_processes_replaces_existing_mapping`，
//!    URL 从 `/shelves/{id}/processes` 改为 `/prod/shelf-processes/{shelf_id}`）
//! 2. `list_all_mappings_returns_active_shelf_mappings` —— `GET /prod/shelf-processes`
//!    全集查询（新 URL，原 shelf 域 `GET /shelves/processes` 从未被集成测试覆盖）
//! 3. `set_shelf_processes_rejects_unknown_process` —— items 里 process_id 不存在
//!    → 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND`（HTTP 404）
//! 4. `old_shelf_process_paths_are_gone` —— 硬切验证：3 个旧路径按 URI 钉死状态码
//!    （`GET /shelves/processes` → 400；`GET|POST /shelves/{id}/processes` → 404）
//!
//! ## fixture 选择（2026-10-02 判定）
//! 用 **production fixture**（`load_production_fixture`）而非 shelf fixture：
//! 本测试已迁入 `production` test binary（`tests/production/main.rs` 加
//! `mod shelf_process;`），与同目录 `process_chain.rs` / `work_type.rs` 一致走本域
//! 权威 fixture。`load_production_fixture` 内部先 `load_part_fixture`，因此
//! `PartFixture::PRODUCTION_SHELF_ID`（FX-SH-PROD，PRODUCTION 区 active 架）与
//! `ProductionFixture::PROCESS_A_ID` / `PROCESS_B_ID` 全部可用，无需再叠一层
//! shelf fixture（叠了也只是多出 1 个 INSPECTION 架 + 1 工序，对本组断言无贡献，
//! 反而让「用哪套 fixture」变模糊）。
//!
//! 与原测试的差异（原测试用 `POST /shelves` 现造一个干净 INSPECTION 架）：本组
//! 直接用 fixture 预置的 `PRODUCTION_SHELF_ID`，它**带 1 条既有 active 映射**
//! （part.sql 段 `9000000000000000025`：FX-SH-PROD → FX-PROC-A）。起点非空让
//! 「整组替换」被测得更强（第一笔 set 就要软删既有行），断言与原测试一致。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database，DB 间 schema 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 全部用 MANAGER（`POST` 写路径要求 M-only）；SHELF_ACCOUNT scope 场景另由
//! `tests/part` 覆盖。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_test_support::{
    PartFixture, ProductionFixture, json_request, load_production_fixture, login_token, send,
    test_app, test_pool, test_state,
};

// ===========================================================================
//  Bootstrap helpers（PR13 Phase F 风格，勿本地重复声明 send / json_request…）
// ===========================================================================

/// 起一份 fresh database + 加载 production fixture + 以 MANAGER 身份登录。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 直插一个 INHOUSE t_process 工序（`POST /prod/shelf-processes/{id}` 要校验
/// process_id 存在；测试按需造不同 code / name）。
///
/// production fixture 只预置 FX-NA / FX-NB 两个工序，本组测试要 3 个（P1/P2/P3）
/// 才能验证「整组替换后剩 2 条 + sort_order 顺序」，故第三个就地直插 —— 沿
/// `tests/shelf/api.rs::insert_test_process` 同形（该 helper 随 mapping 测试一起
/// 迁到本文件，shelf 域不再有 mapping 端点故不再需要它）。
async fn insert_test_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
        id,
        code,
        name,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 整组替换映射：先 set [P1]，再 set [P2, P3] 后旧映射 (P1) 软删、
/// 新映射 (P2+P3) 在场（按 sort_order 排序）。
#[tokio::test]
async fn set_shelf_processes_replaces_existing_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 1. 用 fixture 预置的 PRODUCTION 货架（自带 1 条既有 active 映射）
    let shelf_id = PartFixture::PRODUCTION_SHELF_ID;
    let shelf_id_str = shelf_id.to_string();

    // 2. 准备 3 个工序
    let p1 = insert_test_process(&pool, "P-MAP-1", "Map-Process-1").await;
    let p2 = insert_test_process(&pool, "P-MAP-2", "Map-Process-2").await;
    let p3 = insert_test_process(&pool, "P-MAP-3", "Map-Process-3").await;

    // 3. 第一次 set_shelf_processes —— 仅含 [P1]
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id_str}"),
            Some(json!({
                "items": [
                    { "process_id": p1.to_string(), "sort_order": 0 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "first set processes: {env2}");
    assert_eq!(env2["code"], 0);
    assert!(
        env2["data"].is_null(),
        "set 端点返回 data: null（沿用现状契约）; got: {env2}"
    );

    // 4. 第二次 set_shelf_processes —— 替换为 [P2, P3]
    let (s3, env3) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id_str}"),
            Some(json!({
                "items": [
                    { "process_id": p2.to_string(), "sort_order": 0 },
                    { "process_id": p3.to_string(), "sort_order": 1 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::OK, "second set processes: {env3}");
    assert_eq!(env3["code"], 0);

    // 5. 读 `GET /prod/shelf-processes/{shelf_id}` —— 应只剩 P2, P3 (按 sort_order)
    let (s4, env4) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/prod/shelf-processes/{shelf_id_str}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s4, StatusCode::OK, "list per-shelf processes: {env4}");
    let items = env4["data"]["items"].as_array().expect("items[]");
    assert_eq!(
        items.len(),
        2,
        "after replace, mapping should have exactly 2 entries; got: {env4}"
    );
    // sort_order: P2=0, P3=1
    let pids: Vec<String> = items
        .iter()
        .map(|it| it["process_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(pids, vec![p2.to_string(), p3.to_string()]);
    assert_eq!(items[0]["shelf_id"], shelf_id_str);
    assert_eq!(items[0]["sort_order"], 0);
    assert_eq!(items[1]["sort_order"], 1);

    // 6. DB 侧：P1 mapping 行已软删
    let row: Option<(Option<chrono::NaiveDateTime>,)> = sqlx::query_as(
        "SELECT deleted_at FROM t_shelf_process WHERE shelf_id = $1 AND process_id = $2",
    )
    .bind(shelf_id)
    .bind(p1)
    .fetch_optional(&pool)
    .await
    .expect("query old mapping deleted_at");
    let deleted_at = row.expect("old mapping row exists").0;
    assert!(
        deleted_at.is_some(),
        "old mapping row should be soft-deleted; got None"
    );
}

/// `GET /prod/shelf-processes` 全集查询：返回所有 active shelf 的映射行
/// （含 fixture 预置的 PRODUCTION 架与 INSPECTION 架）。
#[tokio::test]
async fn list_all_mappings_returns_active_shelf_mappings() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        app.clone(),
        json_request("GET", "/prod/shelf-processes", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list all mappings: {env}");
    assert_eq!(env["code"], 0);

    let items = env["data"]["items"].as_array().expect("items[]");
    // part fixture 预置 1 条映射：PRODUCTION 架(15) → FX-PROC-A(12)
    assert_eq!(items.len(), 1, "expected only the fixture mapping: {env}");
    assert_eq!(
        items[0]["shelf_id"],
        PartFixture::PRODUCTION_SHELF_ID.to_string()
    );
    assert_eq!(items[0]["shelf_code"], "FX-SH-PROD");
    assert_eq!(items[0]["process_id"], PartFixture::PROCESS_ID.to_string());
    assert_eq!(items[0]["process_code"], "FX-PROC-A");
    // 全集端点不带 sort_order 字段（AllShelfProcessMappingItem 无该列）
    assert!(
        items[0].get("sort_order").is_none(),
        "all-mappings item 不含 sort_order; got: {env}"
    );

    // 单架端点的 item 带 sort_order
    let (s1, env1) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/shelf-processes/{}", PartFixture::PRODUCTION_SHELF_ID),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "list per-shelf: {env1}");
    let one = env1["data"]["items"].as_array().expect("items[]");
    assert_eq!(one.len(), 1, "per-shelf should have 1 row: {env1}");
    assert_eq!(one[0]["sort_order"], 0);
}

/// items 里有不存在的 process_id → 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND`
/// （HTTP 404），且不写任何 mapping（事务回滚）。
#[tokio::test]
async fn set_shelf_processes_rejects_unknown_process() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    let shelf_id = PartFixture::PRODUCTION_SHELF_ID;
    let shelf_id_str = shelf_id.to_string();
    // 999999999999 不存在的工序 id（雪花 ID 量级但库内无此行）
    let ghost = 999_999_999_999_i64;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id_str}"),
            Some(json!({
                "items": [
                    { "process_id": ghost.to_string(), "sort_order": 0 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unknown process: {env}");
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20505,
        "expected BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND; got: {env}"
    );

    // 事务回滚：既有的 fixture 映射行仍在（未被软删）
    let still_active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_shelf_process WHERE shelf_id = $1 AND deleted_at IS NULL",
    )
    .bind(shelf_id)
    .fetch_one(&pool)
    .await
    .expect("count active mappings");
    assert_eq!(
        still_active, 1,
        "rejected set must not soft-delete existing mappings; got {still_active}"
    );
}

/// 硬切验证：3 个旧路径按 URI **逐个钉死**期望状态码（无 alias，沿 2026-09-19
/// prod 聚合先例）。
///
/// 注意响应体**不是** `R` 信封，故本测试绕过 `send()`（它会
/// `serde_json::from_str` 解析信封而 panic），直接 `app.oneshot(req)` 断状态码。
///
/// 期望值（2026-10-02 review 第 1 轮 M-4 加固：原先只断 `4xx`，误加 403 角色守卫
/// 或误返 405 也会绿，故按 URI 钉死具体码）：
/// - `GET /shelves/processes` → **400**：`processes` 落到 shelf 域 `/{id}` 路由，
///   `Path<i64>` 解析失败被 axum 拒为 400 纯文本（**不是** 404）
/// - `GET|POST /shelves/{id}/processes` → **404**：该 route 已从 router 整体删除，
///   无任何匹配
#[tokio::test]
async fn old_shelf_process_paths_are_gone() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let shelf_id_str = PartFixture::PRODUCTION_SHELF_ID.to_string();
    let cases: [(&str, String, StatusCode); 3] = [
        (
            "GET",
            "/shelves/processes".to_string(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "GET",
            format!("/shelves/{shelf_id_str}/processes"),
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            format!("/shelves/{shelf_id_str}/processes"),
            StatusCode::NOT_FOUND,
        ),
    ];
    for (method, uri, expected) in cases {
        let req = json_request(method, &uri, Some(json!({ "items": [] })), Some(&token));
        let resp = app.clone().oneshot(req).await.expect("oneshot");
        let status = resp.status();
        assert_eq!(
            status, expected,
            "old path {method} {uri} must be gone (no alias) and return {expected}; got: {status}"
        );
    }
}
