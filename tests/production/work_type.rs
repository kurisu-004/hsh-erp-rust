//! work_type 域端到端集成测试
//!
//! ## 覆盖（Task 5 work_type CRUD + mapping）
//! 1. `create_work_type_then_set_processes_then_soft_delete_in_use` — 完整 happy path +
//!    soft-delete-in-use 拒：创建工种 → set 工序映射 → 试图软删 → 期望 20903 BIZ_WORK_TYPE_IN_USE。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（写路径要求 M-only，按设计 §6.1 用 M 即可）。
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! `login_manager` 通用 helper，统一走 `use hsh_erp_test_support::{...}` +
//! `bootstrap_as_manager()` + `load_production_fixture(&pool)`。剩余 helper
//! `insert_work_type_process_mapping` 是本 sub-file 独享的 raw SQL 构造
//! （绕开业务 set_work_type_processes 端点），保留为本地 fn。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::{
    ProductionFixture, json_request, load_production_fixture, login_token, send, test_app,
    test_pool, test_state,
};

// ===========================================================================
//  动态 fixture helper（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::seed_process` 引入，因 fixtures.rs
//  本轮被删，复制到本地（同形 SQL：INHOUSE 类别 t_process 直插）。
// ===========================================================================

/// 插一个 INHOUSE 类别的 `t_process` 工序，返回 process_id。
async fn seed_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_test_support::pool_snowflake;

    let snowflake = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 production fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。后续测试可直接 `pool` 跑 query!、
/// `app.clone()` 多次 `send`、token 直接拼到 bearer header；`fx` 暴露
/// `part_manager_username` / 预制常量 ID 等强类型句柄。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  work_type 域独享 helper（绕开业务 set_work_type_processes 端点）
// ===========================================================================

/// 直插 `t_work_type_process` 行（无业务软删：`deleted_at` 留默认 NULL）。
///
/// 模拟 `set_work_type_processes` 写入后的状态，避免依赖同任务的 mapping 端点语义
/// 把「create work_type + soft-delete-in-use 拒」测试与「mapping 端点 happy path」耦在一起。
async fn insert_work_type_process_mapping(pool: &PgPool, wt_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
        id,
        wt_id,
        p_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_work_type_process");
}

// ===========================================================================
// Tests
// ===========================================================================

/// 完整 happy path：
/// 1. create work_type（含 description + sort_order + max_held_batches）
/// 2. 直接 raw SQL 插 `t_work_type_process` 行（绕开业务 set_work_type_processes，
///    仅为了制造引用，避免与 mapping 端点的语义耦合）
/// 3. soft-delete → 期望 409 + 20903 `BIZ_WORK_TYPE_IN_USE`
///    （service 层 UNION ALL 查 t_worker.work_type_id + t_work_type_process 引用）
#[tokio::test]
async fn create_work_type_then_set_processes_then_soft_delete_in_use() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 1) Create work type
    let (s_create, env_create) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/work-types",
            Some(json!({
                "code": "WT-CNC",
                "name": "CNC Operator",
                "description": "5-axis CNC",
                "sort_order": 10,
                "max_held_batches": 3,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_create,
        StatusCode::CREATED,
        "create work type: {env_create}"
    );
    assert_eq!(env_create["code"], 0);
    let wt_id_str = env_create["data"]["id"].as_str().unwrap().to_string();
    let wt_id: i64 = wt_id_str.parse().unwrap();
    assert_eq!(env_create["data"]["code"], "WT-CNC");
    assert_eq!(env_create["data"]["name"], "CNC Operator");
    assert_eq!(env_create["data"]["description"], "5-axis CNC");
    assert_eq!(env_create["data"]["sort_order"], 10);
    assert_eq!(env_create["data"]["max_held_batches"], 3);

    // 2) 直接 raw SQL 插 `t_work_type_process` 行：seed 2 个工序 + 插映射
    let p1 = seed_process(&pool, "WT-CNC-P1", "CNC step 1").await;
    let p2 = seed_process(&pool, "WT-CNC-P2", "CNC step 2").await;
    insert_work_type_process_mapping(&pool, wt_id, p1).await;
    insert_work_type_process_mapping(&pool, wt_id, p2).await;

    // 3) Soft-delete → 期望 409 + 20903
    let (s_del, env_del) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/work-types/{wt_id_str}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_del,
        StatusCode::CONFLICT,
        "soft-delete in-use work type should return 409; got {env_del}"
    );
    assert_eq!(
        env_del["code"].as_i64().unwrap(),
        20903,
        "expected BIZ_WORK_TYPE_IN_USE; got envelope: {env_del}"
    );
}