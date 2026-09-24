//! outsource company 域集成测试（Phase 2 2026-09-13）
//!
//! 覆盖：
//! - list: 创建 3 个公司 + name_like 过滤 + is_active 过滤
//! - get: 详情含 process_ids 反查
//! - create: name 必填 + 重名 409 + 可选 process_ids 注入
//! - update: OCC 版本冲突 + 部分字段
//! - soft-delete: 仍映射工序时 409
//! - list-by-process: 按 process 反查 active 公司
//! - set-processes: 整体替换（delete-then-insert）
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! `login_manager` 通用 helper，统一走
//! `use hsh_erp_test_support::{...}` + `bootstrap_as_manager()` +
//! `load_outsource_fixture(&pool)`。保留：
//! - `seed_outsource_process`：company 域独享（9 个场景需要不同 OUTSOURCE
//!   工序 code，按需用 sqlx::query 直插；fixture 预置的 FX-OPROC-A 仅作
//!   baseline 共享）；
//! - 域独享 helper 不从 `fixtures` 模块 `use`（Phase H gate 5 禁止）；
//!   本地 helper 用 `sqlx::query` 直插与 `fixtures::seed_process` 同形 SQL
//!   （columns / defaults 全部对齐 migration 003 的 t_process schema，
//!   category='OUTSOURCE'）。
//!
//! ## 不预置 t_outsource_company
//! 各场景需要不同 name / is_active / contact 等字段；预置 1 行 FX-OC-001
//! 不与测试自建 company 撞（uk_t_outsource_company_name 仅约束 active 同名
//! 唯一），但绝大多数场景希望公司列表干净从 0 起算，因此各 sub-file 用
//! 本地 `insert_outsource_company` 插自定义行。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, send, test_app,
    test_pool, test_state,
};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 outsource fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。后续测试可直接 `pool` 跑 query!、
/// `app.clone()` 多次 `send`、token 直接拼到 bearer header；`fx` 暴露
/// `part_manager_username` / 预制常量 ID 等强类型句柄（绝大多数 company
/// 域测试用本地 `seed_outsource_process` 自建 OUTSOURCE 工序，仅
/// `outsource_process_id` / `outsource_company_id` 作为 baseline 句柄备查）。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  company 域独享 helpers（绕开 fixtures::seed_process 因为 Phase H gate 5
//  禁止从 `fixtures` 模块 use 任何动态 helper）
// ===========================================================================

/// 直插一个 OUTSOURCE 类别 `t_process` 工序。
///
/// 与 `fixtures::seed_process` 同形 SQL，但 category='OUTSOURCE'（fixture 版
/// 走 INHOUSE，company 域必须用 OUTSOURCE）。
async fn seed_outsource_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert OUTSOURCE process");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_outsource_company_happy_path() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "Acme 加工厂", "is_active": true})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["name"], "Acme 加工厂");
    assert_eq!(env["data"]["is_active"], true);
    assert_eq!(env["data"]["version"], 0);
    assert_eq!(env["data"]["processes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn create_outsource_company_duplicate_returns_21202() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "SameName"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "SameName"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "duplicate: {env2}");
    assert_eq!(env2["code"].as_i64().unwrap(), 21202);
}

/// 2026-09-15 fix-outsource-409 回归测试：
/// 同名第二次创建必须返回 409 而不是 500。Code 可以是：
/// - 21202（应用层 pre-check 命中：`OutsourceCompanyRepo::get_by_name` 找到 active 行）
/// - 21214（DB `uk_t_outsource_company_name` 兜底：pre-check 漏网，INSERT 撞部分唯一索引）
/// 单线程顺序测试通常命中前者，但保证两种路径下都不返 500。
#[tokio::test]
async fn create_outsource_company_duplicate_returns_409() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "E2E_TEST_DUP_COMPANY"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "E2E_TEST_DUP_COMPANY"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "duplicate must be 409 not 500: {env2}"
    );
    let code = env2["code"].as_i64().unwrap();
    assert!(
        code == 21202 || code == 21214,
        "duplicate code must be 21202 or 21214, got {code}; full env: {env2}"
    );
}

#[tokio::test]
async fn create_outsource_company_with_process_ids_creates_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "PROC-O-1", "外协工序1").await;
    let p2 = seed_outsource_process(&pool, "PROC-O-2", "外协工序2").await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({
                "name": "ProcMapping Co",
                "process_ids": [p1.to_string(), p2.to_string()]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create with procs: {env}");
    let cid = env["data"]["id"].as_str().unwrap().to_string();
    let procs = env["data"]["processes"].as_array().unwrap();
    assert_eq!(procs.len(), 2);
    assert_eq!(procs[0]["process_id"].as_str().unwrap(), p1.to_string());
    assert_eq!(procs[1]["process_id"].as_str().unwrap(), p2.to_string());

    // by-process 也能查到
    let (_, env_by) = send(
        app,
        json_request(
            "GET",
            &format!("/outsource-companies/by-process/{p1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let items = env_by["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), cid);
}

#[tokio::test]
async fn update_outsource_company_version_conflict_returns_40901() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "VC Co"})),
            Some(&token),
        ),
    )
    .await;
    let cid = env_c["data"]["id"].as_str().unwrap().to_string();

    // 故意传错 version
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({"name": "VC Co New", "version": 99})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "vc: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40901);
}

#[tokio::test]
async fn soft_delete_outsource_company_in_use_returns_21205() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "PROC-INUSE", "外协INUSE").await;
    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({
                "name": "InUse Co",
                "process_ids": [p1.to_string()]
            })),
            Some(&token),
        ),
    )
    .await;
    let cid = env_c["data"]["id"].as_str().unwrap().to_string();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "in-use: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21205);
}

#[tokio::test]
async fn list_outsource_companies_name_like_and_is_active_filter() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    // 3 个公司：A 激活、B 激活、C 停用
    for (n, active) in [("AAAA Inc", true), ("BBBB Co", true), ("CCCC Ltd", false)] {
        let (s, _) = send(
            app.clone(),
            json_request(
                "POST",
                "/outsource-companies",
                Some(json!({"name": n, "is_active": active})),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }

    // name_like=AA → 1（fixture FX-OC-001 不含 "AA" 子串，命中 0）
    let (_, env1) = send(
        app.clone(),
        json_request(
            "GET",
            "/outsource-companies?name_like=AA",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["data"]["total"].as_i64().unwrap(), 1);

    // is_active=true → 3（fixture FX-OC-001 active + AAAA/BBBB active 共 3 行）
    // 2026-09-24 PR13 Phase H：fixture 预置 FX-OC-001（is_active=true），
    // 与原测试 2 行合计 3 行。原版断言 2 改为 3。
    let (_, env2) = send(
        app.clone(),
        json_request(
            "GET",
            "/outsource-companies?is_active=true",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env2["data"]["total"].as_i64().unwrap(), 3);
}

#[tokio::test]
async fn set_outsource_company_processes_replaces_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "SP-1", "sp1").await;
    let p2 = seed_outsource_process(&pool, "SP-2", "sp2").await;
    let p3 = seed_outsource_process(&pool, "SP-3", "sp3").await;
    // 创建时只挂 p1
    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "SetProc Co", "process_ids": [p1.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    let cid = env_c["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(env_c["data"]["processes"].as_array().unwrap().len(), 1);

    // 整体替换为 [p2, p3]
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/processes"),
            Some(json!({"process_ids": [p2.to_string(), p3.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "set-processes: {env}");
    let procs = env["data"]["processes"].as_array().unwrap();
    assert_eq!(procs.len(), 2);
    let ids: Vec<String> = procs
        .iter()
        .map(|p| p["process_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&p2.to_string()));
    assert!(ids.contains(&p3.to_string()));
    // p1 应被清除
    assert!(!ids.contains(&p1.to_string()));
}

#[tokio::test]
async fn list_outsource_companies_by_process_filters_inactive() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p = seed_outsource_process(&pool, "BYP", "byp").await;
    // active
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "Active Co", "process_ids": [p.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    // inactive
    let (_, env_i) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(
                json!({"name": "Inactive Co", "is_active": false, "process_ids": [p.to_string()]}),
            ),
            Some(&token),
        ),
    )
    .await;
    let inactive_id = env_i["data"]["id"].as_str().unwrap().to_string();

    let (_, env) = send(
        app,
        json_request(
            "GET",
            &format!("/outsource-companies/by-process/{p}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let items = env["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_ne!(items[0]["id"].as_str().unwrap(), inactive_id);
}
