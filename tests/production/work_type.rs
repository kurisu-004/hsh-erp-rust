//! work_type 域端到端集成测试
//!
//! ## 覆盖
//! 1. `create_work_type_then_set_processes_then_soft_delete_in_use` — 创建工种 →
//!    raw SQL 造引用 → 软删被拒 20903 `BIZ_WORK_TYPE_IN_USE`。
//! 2. `set_processes_replaces_group_and_get_returns_only_active` — 2026-10-02 新增：
//!    `POST`↔`GET` 整组替换往返（set [P1,P2] → set [P2] → set []），钉死「读路径只返回
//!    active 映射」；末尾顺带断言工种列表的 `process_ids` 出参同步收窄。
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

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
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

/// 直插一条 **active** 的 `t_work_type_process` 行（`deleted_at` 留默认 NULL）。
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
/// 2. 直接 raw SQL 插 `t_work_type_process` 行（**刻意绕开**业务端点
///    `POST /prod/work-types/{id}/processes`：本测试只验 soft-delete-in-use 拒，
///    走端点会把 mapping 端点的语义耦进来。端点往返由
///    `set_processes_replaces_group_and_get_returns_only_active` 单独覆盖）
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

/// set→get 往返：整组替换后 `GET` 只能看到当前 active 映射。
///
/// 覆盖 4 步：
/// 1. 建工种 → `POST [P1, P2]` → `GET` 恰为 `[P1, P2]`（按 process_code 逐个断言）
/// 2. `POST [P2]` → `GET` 恰为 `[P2]`（**本次回归的核心断言**：P1 已软删，
///    读路径不过滤 `deleted_at` 时它会随软删行一起被返回）
/// 3. `POST {"items": []}`（key 不可省略）→ `GET` 为空数组
/// 4. `GET /prod/work-types` 的 `process_ids` 出参同步收窄（批量补全 SQL，
///    与第 1~3 步是不同查询）
#[tokio::test]
async fn set_processes_replaces_group_and_get_returns_only_active() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 0) 建工种
    let (s_create, env_create) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/work-types",
            Some(json!({
                "code": "WT-MAP",
                "name": "Mapping Work Type",
                "sort_order": 20,
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
    let wt_id_str = env_create["data"]["id"].as_str().unwrap().to_string();
    let mapping_uri = format!("/prod/work-types/{wt_id_str}/processes");

    // 1) 首次 set [P1, P2] → GET 恰为 [P1, P2]
    let p1 = seed_process(&pool, "WT-MAP-P1", "Mapping step 1").await;
    let p2 = seed_process(&pool, "WT-MAP-P2", "Mapping step 2").await;
    let (s_set1, env_set1) = send(
        app.clone(),
        json_request(
            "POST",
            &mapping_uri,
            Some(json!({
                "items": [
                    { "process_id": p1.to_string(), "sort_order": 0 },
                    { "process_id": p2.to_string(), "sort_order": 1 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_set1, StatusCode::OK, "first set processes: {env_set1}");
    assert_eq!(env_set1["code"], 0);

    let (s_get1, env_get1) = send(
        app.clone(),
        json_request("GET", &mapping_uri, None, Some(&token)),
    )
    .await;
    assert_eq!(s_get1, StatusCode::OK, "first list mapping: {env_get1}");
    let items1 = env_get1["data"]["items"].as_array().expect("items[]");
    let codes1: Vec<&str> = items1
        .iter()
        .map(|it| it["process_code"].as_str().unwrap())
        .collect();
    assert_eq!(
        codes1,
        vec!["WT-MAP-P1", "WT-MAP-P2"],
        "first set [P1, P2] must be returned as-is (sort_order order); got: {env_get1}"
    );
    assert_eq!(items1[0]["process_id"], p1.to_string());
    assert_eq!(items1[1]["process_id"], p2.to_string());
    assert_eq!(items1[0]["work_type_id"], wt_id_str);
    assert_eq!(items1[0]["sort_order"], 0);
    assert_eq!(items1[1]["sort_order"], 1);

    // 2) 再次 set [P2] → GET 恰为 [P2]（P1 软删行不得回流）
    let (s_set2, env_set2) = send(
        app.clone(),
        json_request(
            "POST",
            &mapping_uri,
            Some(json!({
                "items": [
                    { "process_id": p2.to_string(), "sort_order": 0 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_set2, StatusCode::OK, "second set processes: {env_set2}");
    let (s_get2, env_get2) = send(
        app.clone(),
        json_request("GET", &mapping_uri, None, Some(&token)),
    )
    .await;
    assert_eq!(s_get2, StatusCode::OK, "second list mapping: {env_get2}");
    let items2 = env_get2["data"]["items"].as_array().expect("items[]");
    let codes2: Vec<&str> = items2
        .iter()
        .map(|it| it["process_code"].as_str().unwrap())
        .collect();
    assert_eq!(
        codes2,
        vec!["WT-MAP-P2"],
        "整组替换后 GET 只能看到 active 映射（被取消勾选的 P1 不得回流）; got: {env_get2}"
    );
    assert_eq!(items2[0]["process_id"], p2.to_string());

    // 3) set 空 items（key 不可省略）→ GET 空数组
    let (s_set3, env_set3) = send(
        app.clone(),
        json_request(
            "POST",
            &mapping_uri,
            Some(json!({ "items": [] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_set3, StatusCode::OK, "clear mapping: {env_set3}");
    let (s_get3, env_get3) = send(
        app.clone(),
        json_request("GET", &mapping_uri, None, Some(&token)),
    )
    .await;
    assert_eq!(s_get3, StatusCode::OK, "list after clear: {env_get3}");
    let items3 = env_get3["data"]["items"].as_array().expect("items[]");
    assert!(
        items3.is_empty(),
        "空 items = 清空全部映射，GET 应返回空数组; got: {env_get3}"
    );

    // 4) 工种列表的 process_ids 出参同步收窄（list_by_work_types_batch 批量补全）
    let (s_list, env_list) = send(
        app.clone(),
        json_request("GET", "/prod/work-types", None, Some(&token)),
    )
    .await;
    assert_eq!(s_list, StatusCode::OK, "list work types: {env_list}");
    let listed = env_list["data"]["items"]
        .as_array()
        .expect("items[]")
        .iter()
        .find(|it| it["id"] == wt_id_str)
        .expect("created work type must be listed")
        .clone();
    let pids: Vec<&str> = listed["process_ids"]
        .as_array()
        .expect("process_ids[]")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        pids.is_empty(),
        "清空映射后工种列表的 process_ids 应同步为空（不过滤软删行会残留 P1/P2）; got: {pids:?}"
    );
}
