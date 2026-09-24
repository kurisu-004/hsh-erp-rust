//! shelf 域端到端集成测试
//!
//! ## 覆盖（Task 3 shelf CRUD + picker + mapping）
//! 1. `create_shelf_then_deactivate_with_in_use_part_fails` — `deactivate` 拒绝
//!    被 IN_PROCESS/INSPECTION/REPAIRING 零件引用的货架 → 20503 BIZ_SHELF_IN_USE。
//! 2. `create_then_get_shelf_round_trip` — happy path：create → get → 含 location 字段。
//! 3. `set_shelf_processes_replaces_existing_mapping` — mapping 端点整组替换。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（POST /shelves 写路径要求 M-only，按设计 §6.1 用 M 即可）。
//!
//! ## 直插 t_part 的说明
//! brief 的 20503 测试需要在 deactivate 前插一个 t_part.current_holder_id =
//! shelf_id 且 status IN ('IN_PROCESS','INSPECTION','REPAIRING') 的工单。本
//! 测试**没有**借助 part 域 CRUD（part 域自身不在 Task 3 范围内），而是直接
//! SQL INSERT 落表 —— 与 `customer_api.rs` / `process_api.rs` 的同形 fixture 思路一致。
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 2026-09-24 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` /
//! `setup` / 通用 `login_manager` helper，统一走
//! `use hsh_erp_test_support::{...}` + `bootstrap_as_manager()` +
//! `load_shelf_fixture(&pool)`。保留：
//! - `insert_part_held_by_shelf`：shelf 域独享（绕开 part CRUD 直插 t_part +
//!   t_part_batch；PR-2 已把 `current_holder_id` 等批次依附列迁移到 t_part_batch，
//!   故同时插 batch 行让 `ShelfRepo::count_in_use_parts` 命中真相源路径）
//! - `insert_test_process`：shelf 域独享（mapping 端点要校验 process_id 存在；
//!   测试按需造不同 code / name，不复用 fixtures::seed_process）

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    ShelfFixture, json_request, load_shelf_fixture, login_token, send, test_app, test_pool,
    test_state,
};

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 shelf fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ShelfFixture) {
    let pool = test_pool().await;
    let fx = load_shelf_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ShelfFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  shelf fixture helpers（shelf 域独享，跨 binary 不迁移）
// ===========================================================================

/// 直插一个 `t_part` + `t_part_batch` 行（批次 `current_holder_id = shelf_id`、
/// `location = 'PRODUCTION_SHELF'`、`status = IN_PROCESS`）让 `deactivate` 的
/// 「被 IN_PROCESS/INSPECTION/REPAIRING 批次引用」分支触发 20503
/// `BIZ_SHELF_IN_USE`。绕开 part 域 CRUD（part CRUD 不是本任务范畴）。
///
/// 2026-09-16 PR-2（migration 027）：t_part 删 `current_holder_id`，「该 shelf 持有」
/// 改查 t_part_batch 真相源（status + holder + location 三维核对），fixture 同步改写。
async fn insert_part_held_by_shelf(pool: &PgPool, shelf_id: i64, status: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let batch_id = snowflake.next_id();
    let now = now_naive();
    // 2026-09-16 PR-2（migration 027）：t_part 删 `current_holder_id` 等批次依附列；
    // INSERT 列名与 VALUES 占位符同步移除 `0, 0`（unit_price/total_price 不再写）。
    sqlx::query!(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, customer_id, status, version, \
         created_at, updated_at) \
         VALUES ($1, 'TEST-NAME', 'TEST-DWG', 'TEST-APPLICANT', 1, \
                 CURRENT_DATE, CURRENT_DATE, $2, $3, 0, $4, $4)",
        id,
        // 用伪 customer_id (1L)；truncate CASCADE 后 customer sequence 从 1 开始。
        1_i64,
        status,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part held by shelf");
    // 同步插 t_part_batch（active + location='PRODUCTION_SHELF' + holder=shelf），
    // 让 ShelfRepo::count_in_use_parts 命中 PR-2 真相源路径。
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 1, $3, 'PRODUCTION_SHELF', $4, 0, $5, $5)",
        batch_id,
        id,
        status,
        shelf_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part_batch held by shelf");
    id
}

/// 直插一个 INHOUSE t_process 工序（mapping 端点要校验 process_id 存在）。
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
// Tests
// ===========================================================================

/// 货架创建 + 详情往返：`location` 字段必须出现在 response 里。
#[tokio::test]
async fn create_then_get_shelf_round_trip() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s_create, env_create) = send(
        app.clone(),
        json_request(
            "POST",
            "/shelves",
            Some(json!({
                "code": "S-CRT-01",
                "name": "Create-Shelf-01",
                "zone": "PRODUCTION",
                "location": "Aisle-A-01",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_create,
        StatusCode::CREATED,
        "create shelf should return 201; got: {env_create}"
    );
    assert_eq!(env_create["code"], 0);
    let shelf_id = env_create["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        env_create["data"]["location"].as_str().unwrap(),
        "Aisle-A-01",
        "create response must include location; got: {env_create}"
    );
    assert_eq!(env_create["data"]["zone"], "PRODUCTION");
    assert_eq!(env_create["data"]["is_active"], true);

    let (s_get, env_get) = send(
        app,
        json_request("GET", &format!("/shelves/{shelf_id}"), None, Some(&token)),
    )
    .await;
    assert_eq!(
        s_get,
        StatusCode::OK,
        "get shelf should return 200; got: {env_get}"
    );
    assert_eq!(env_get["code"], 0);
    assert_eq!(env_get["data"]["id"].as_str().unwrap(), shelf_id);
    assert_eq!(env_get["data"]["code"], "S-CRT-01");
    assert_eq!(env_get["data"]["location"].as_str().unwrap(), "Aisle-A-01");
    let _ = pool;
}

/// `deactivate` 拒绝被 IN_PROCESS/INSPECTION/REPAIRING 零件引用的货架
/// → 20503 `BIZ_SHELF_IN_USE`（与 brief Step 1 一致）。
#[tokio::test]
async fn create_shelf_then_deactivate_with_in_use_part_fails() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 1. 创建 PRODUCTION 货架（带 location）
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/shelves",
            Some(json!({
                "code": "S-PROD-01",
                "name": "Production-01",
                "zone": "PRODUCTION",
                "location": "Aisle-A-01",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "create shelf: {env1}");
    let shelf_id_str = env1["data"]["id"].as_str().unwrap().to_string();
    let shelf_id: i64 = shelf_id_str.parse().unwrap();
    assert_eq!(env1["data"]["location"].as_str().unwrap(), "Aisle-A-01");

    // 2. 直插一个 t_part：current_holder_id = shelf_id，status = IN_PROCESS
    insert_part_held_by_shelf(&pool, shelf_id, "IN_PROCESS").await;

    // 3. 试图 deactivate → 期望 409 / 20503
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/shelves/{shelf_id_str}/deactivate"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "deactivate with in-use part should return 409; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20503,
        "expected BIZ_SHELF_IN_USE; got: {env2}"
    );
}

/// `set_shelf_processes` 整组替换映射：先 set [P1]，再 set [P2, P3] 后
/// 旧映射 (P1) 软删、新映射 (P2+P3) 在场。
#[tokio::test]
async fn set_shelf_processes_replaces_existing_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 1. 创建 INSPECTION 货架
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/shelves",
            Some(json!({
                "code": "S-INSP-01",
                "name": "Inspection-01",
                "zone": "INSPECTION",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "create shelf: {env1}");
    let shelf_id_str = env1["data"]["id"].as_str().unwrap().to_string();
    let shelf_id: i64 = shelf_id_str.parse().unwrap();

    // 2. 准备 3 个工序
    let p1 = insert_test_process(&pool, "P-MAP-1", "Map-Process-1").await;
    let p2 = insert_test_process(&pool, "P-MAP-2", "Map-Process-2").await;
    let p3 = insert_test_process(&pool, "P-MAP-3", "Map-Process-3").await;

    // 3. 第一次 set_shelf_processes —— 仅含 [P1]
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/shelves/{shelf_id_str}/processes"),
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

    // 4. 第二次 set_shelf_processes —— 替换为 [P2, P3]
    let (s3, env3) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/shelves/{shelf_id_str}/processes"),
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

    // 5. 读 `GET /shelves/{id}/processes` —— 应只剩 P2, P3 (按 sort_order)
    let (s4, env4) = send(
        app,
        json_request(
            "GET",
            &format!("/shelves/{shelf_id_str}/processes"),
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