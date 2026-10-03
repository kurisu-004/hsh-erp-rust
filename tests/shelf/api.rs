//! shelf 域端到端集成测试
//!
//! ## 覆盖（Task 3 shelf CRUD + picker）
//! 1. `create_shelf_then_deactivate_with_in_use_part_fails` — `deactivate` 拒绝
//!    被 IN_PROCESS/INSPECTION 零件引用的货架 → 20503 BIZ_SHELF_IN_USE。
//!    （2026-10-01：REPAIRING 降级为 `t_part_batch.is_repairing` 标记列，返修
//!    批次 status 即 IN_PROCESS，仍被本守卫覆盖。）
//! 2. `create_then_get_shelf_round_trip` — happy path：create → get → 含 location 字段。
//! 3. `for_inspection_returns_current_load_as_sum_of_quantity`（2026-10-04）—— picker
//!    for-inspection 必须出 `current_load`（`SUM(quantity)` 件数口径）且空架取 0；
//!    出参若缺该字段，前端会渲染「在架 undefined 件」—— 由本测试兜住。
//!
//! 2026-10-02 域拆分：原第 3 个测试 `set_shelf_processes_replaces_existing_mapping`
//! 连同 `insert_test_process` helper 一起迁到
//! `tests/production/shelf_process.rs`（3 个 mapping 端点搬到 `prod::shelf_process`
//! 子模块，URL 硬切 `/api/v2/prod/shelf-processes/*`，无 alias）。`insert_test_process`
//! 在本域已无其它调用方，故随迁走、不保留副本。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（POST /shelves 写路径要求 M-only，按设计 §6.1 用 M 即可）。
//!
//! ## 直插 t_part 的说明
//! brief 的 20503 测试需要在 deactivate 前插一个 t_part_batch.current_holder_id =
//! shelf_id 且 status IN ('IN_PROCESS','INSPECTION') 的批次。本
//! （2026-10-01：REPAIRING 降级为 is_repairing 标记，返修批次 status 即 IN_PROCESS）
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

use axum::http::StatusCode;
use serde_json::json;
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

/// 直插一个 `t_part` + `t_part_batch` 行（批次 `current_holder_id = shelf_id`）
/// 让货架相关的聚合 / 引用计数命中真相源。绕开 part 域 CRUD（part CRUD 不是本
/// 任务范畴）。
///
/// - `status`：批次与 part 同状态（`'IN_PROCESS'` / `'INSPECTION'` …）。`deactivate`
///   的 20503 `BIZ_SHELF_IN_USE` 走 `status IN ('IN_PROCESS','INSPECTION')` 臂；
///   picker 的 `current_load` 走更大的 status 列表。
/// - `location`：批次位置（`'PRODUCTION_SHELF'` / `'INSPECTION_SHELF'`），须与货架
///   所在区一致 —— `count_in_use_parts` 按 status + holder + location 三维核对。
/// - `quantity`：同时写 `t_part.quantity` 与 `t_part_batch.quantity`。断言
///   `current_load` 件数口径时传 > 1，才能把 `SUM(quantity)` 与 `COUNT(*)` 区分开。
///
/// 返回 part_id。
///
/// 2026-09-16 PR-2（migration 027）：t_part 删 `current_holder_id`，「该 shelf 持有」
/// 改查 t_part_batch 真相源（status + holder + location 三维核对），fixture 同步改写。
async fn insert_part_held_by_shelf(
    pool: &PgPool,
    shelf_id: i64,
    status: &str,
    location: &str,
    quantity: i32,
) -> i64 {
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
         VALUES ($1, 'TEST-NAME', 'TEST-DWG', 'TEST-APPLICANT', $2, \
                 CURRENT_DATE, CURRENT_DATE, $3, $4, 0, $5, $5)",
        id,
        quantity,
        // 用伪 customer_id (1L)；truncate CASCADE 后 customer sequence 从 1 开始。
        1_i64,
        status,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part held by shelf");
    // 同步插 t_part_batch（active + location 匹配货架区 + holder=shelf），让
    // ShelfRepo::count_in_use_parts 与 picker 的 current_load 聚合命中真相源路径。
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, $3, $4, $5, $6, 0, $7, $7)",
        batch_id,
        id,
        quantity,
        status,
        location,
        shelf_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part_batch held by shelf");
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

/// `deactivate` 拒绝被 IN_PROCESS/INSPECTION 零件引用的货架
/// （2026-10-01：REPAIRING 已降级为 `is_repairing` 标记列，返修批次 status 即
/// IN_PROCESS，守卫强度不变）
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
    insert_part_held_by_shelf(&pool, shelf_id, "IN_PROCESS", "PRODUCTION_SHELF", 1).await;

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

/// `GET /shelves/for-inspection` 必须带 `current_load`，且为该架在架批次的
/// `SUM(quantity)`（件数口径）。
///
/// 2026-10-04 回归：出参若缺 `current_load`（数据源退回裸 `TShelf` 列表查询、
/// 不带聚合），前端品检架卡片无 `v-if` 守卫地渲染「在架 N 件」⇒ 每张送检架卡片
/// 显示「在架 **undefined** 件」。本测试是该出参契约的防线：断言会直接失败。
///
/// 断言用 `quantity=3` 的单个批次而非 2 个 `quantity=1`：这样 `SUM(quantity)=3`
/// 与 `COUNT(*)=1` 可区分，把「件数口径」钉死。
#[tokio::test]
async fn for_inspection_returns_current_load_as_sum_of_quantity() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 1. 建一个 INSPECTION 区货架（for-inspection 只看 INSPECTION 区）
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/shelves",
            Some(json!({
                "code": "S-INSP-01",
                "name": "Inspection-01",
                "zone": "INSPECTION",
                "location": "QC-Bay-01",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "create inspection shelf: {env1}");
    let shelf_id: i64 = env1["data"]["id"].as_str().unwrap().parse().unwrap();

    // 2. 另一个 INSPECTION 架，零批次 —— 用于验证 LEFT JOIN 侧的空载取 0
    let (s1b, env1b) = send(
        app.clone(),
        json_request(
            "POST",
            "/shelves",
            Some(json!({
                "code": "S-INSP-02",
                "name": "Inspection-02",
                "zone": "INSPECTION",
                "location": "QC-Bay-02",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s1b,
        StatusCode::CREATED,
        "create 2nd inspection shelf: {env1b}"
    );

    // 3. 直插一个 quantity=3、current_holder_id=该架的 INSPECTION 批次
    insert_part_held_by_shelf(&pool, shelf_id, "INSPECTION", "INSPECTION_SHELF", 3).await;

    // 4. 拉 for-inspection picker，按 code 定位本测试建的 2 个架
    let (s2, env2) = send(
        app,
        json_request("GET", "/shelves/for-inspection", None, Some(&token)),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "for-inspection: {env2}");
    assert_eq!(env2["code"], 0);

    let items = env2["data"]["items"].as_array().unwrap();
    let loaded = items
        .iter()
        .find(|it| it["code"] == "S-INSP-01")
        .unwrap_or_else(|| panic!("S-INSP-01 missing from for-inspection: {env2}"));
    assert_eq!(
        loaded["current_load"].as_i64(),
        Some(3),
        "current_load 必须是 quantity 总和（件数口径）而非批次数; got: {loaded}"
    );
    assert_eq!(loaded["zone"], "INSPECTION");
    // is_recommended 是 for-return 独有的出参，本端点不提供
    assert!(
        loaded.get("is_recommended").is_none(),
        "for-inspection 不应带 is_recommended; got: {loaded}"
    );

    let empty = items
        .iter()
        .find(|it| it["code"] == "S-INSP-02")
        .unwrap_or_else(|| panic!("S-INSP-02 missing from for-inspection: {env2}"));
    assert_eq!(
        empty["current_load"].as_i64(),
        Some(0),
        "空载货架 LEFT JOIN 应取 0（不是 null）; got: {empty}"
    );
}
