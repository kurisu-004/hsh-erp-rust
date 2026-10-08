//! shelf 域端到端集成测试
//!
//! ## 覆盖（Task 3 shelf CRUD + picker）
//! 1. `create_shelf_then_deactivate_with_in_use_part_fails` — `deactivate` 拒绝
//!    被 IN_PROCESS/INSPECTION 零件引用的货架 → 20503 BIZ_SHELF_IN_USE。
//!    （2026-10-01：REPAIRING 降级为 `t_part_batch.is_repairing` 标记列，返修
//!    批次 status 即 IN_PROCESS，仍被本守卫覆盖。）
//! 2. `create_then_get_shelf_round_trip` — happy path：create → get → 含 location 字段。
//! 3. `picker_endpoints_are_gone`（2026-10-10）—— picker 两条端点下线后旧路径 404。
//!    （原 `for_inspection_returns_current_load_as_sum_of_quantity` 随端点下线删除；
//!    `current_load` 的出参契约改由 `list_and_get_expose_capacity_and_current_load`
//!    在 `GET /shelves` 上覆盖。）
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
    ShelfFixture, json_request, load_shelf_fixture, login_token, send, send_raw, test_app,
    test_pool, test_state,
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
    // 2026-10-09：ID 统一从全进程共享 generator 取（`shared_test_snowflake`）——
    // 两个 fresh generator 同 instance 同毫秒各取 seq 0 会撞 `t_part_pkey`（23505），
    // 与「同一个 helper 调几次」无关。
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let batch_id = hsh_erp_test_support::shared_test_snowflake().next_id();
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

/// 2026-10-10：picker 两条端点**下线**，旧路径不再返回货架数据。
///
/// 货架改由服务端按负载自动选（`shared::shelf::select::pick_least_loaded`），
/// 前端不再需要「挑一个架」这个动作，故 `/for-return` 与 `/for-inspection` 一并
/// 删除。
///
/// ## 响应形态：400 而不是 404（必须知道，否则会误判成「端点还在」）
///
/// 本域还挂着 `/{id}`（`Path<i64>`），所以 `/for-return` 现在落进那个 catch-all 并在
/// **Path 提取器**阶段被拒 ⇒ **400 + 纯文本** `Invalid URL: Cannot parse
/// \`for-return\` to a \`i64\``，**不进 `R<T>` 信封**。也就是说「下线」在本 router 下的
/// 实际表现是 400 而不是 404；无论哪种都不是 200、都不会返回货架数据。
#[tokio::test]
async fn picker_endpoints_are_gone() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    for path in ["/shelves/for-return", "/shelves/for-inspection"] {
        let (s, raw) = send_raw(app.clone(), json_request("GET", path, None, Some(&token))).await;
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "{path} 应已下线（落进 /{{id}} 的 Path 提取器 → 400）: {s} {raw}"
        );
        assert!(
            !raw.contains("current_load") && !raw.contains(r#""items""#),
            "{path} 不得再返回 picker 的货架列表: {raw}"
        );
    }
}

/// 2026-10-10：`GET /shelves` 与 `GET /shelves/{id}` 的 `capacity` / `current_load`
/// 两个新出参。
///
/// 断言三件事：
/// 1. `capacity` 是**裸 JSON number**（不是雪花 id 那样的字符串）—— 前端表单控件
///    要拿到 `200` 而不是 `"200"`；
/// 2. `current_load` 是 `SUM(quantity)` 的件数口径（`quantity=3` 的单批次 ⇒ 3，
///    与 `COUNT(*)=1` 可区分）；
/// 3. `load_ratio` **不在**响应里 —— 比例由前端按 `current_load / capacity` 现算，
///    后端发浮点会让前端多一层精度处理（理由见 `vo/shelf.rs` 的注释）。
#[tokio::test]
async fn list_and_get_expose_capacity_and_current_load() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/shelves",
            Some(json!({
                "code": "S-CAP-01",
                "name": "Capacity-01",
                "zone": "PRODUCTION",
                "capacity": 50,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "create shelf: {env1}");
    let shelf_id: i64 = env1["data"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        env1["data"]["capacity"].as_i64(),
        Some(50),
        "create 回显必须是裸 number; got: {env1}"
    );
    assert_eq!(env1["data"]["current_load"].as_i64(), Some(0));

    insert_part_held_by_shelf(&pool, shelf_id, "IN_PROCESS", "PRODUCTION_SHELF", 3).await;

    let (s2, env2) = send(
        app.clone(),
        json_request("GET", "/shelves", None, Some(&token)),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "list shelves: {env2}");
    let row = env2["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["code"] == "S-CAP-01")
        .unwrap_or_else(|| panic!("S-CAP-01 missing from list: {env2}"));
    assert_eq!(row["capacity"].as_i64(), Some(50));
    assert_eq!(
        row["current_load"].as_i64(),
        Some(3),
        "current_load 必须是 quantity 总和（件数口径）; got: {row}"
    );
    assert!(
        row.get("load_ratio").is_none(),
        "load_ratio 不进 ShelfOut（前端现算）；got: {row}"
    );

    let (s3, env3) = send(
        app,
        json_request("GET", &format!("/shelves/{shelf_id}"), None, Some(&token)),
    )
    .await;
    assert_eq!(s3, StatusCode::OK, "get shelf: {env3}");
    assert_eq!(env3["data"]["capacity"].as_i64(), Some(50));
    assert_eq!(env3["data"]["current_load"].as_i64(), Some(3));
}

/// 2026-10-10：`ShelfUpdateRequest.capacity` 的**三态**语义。
///
/// - 缺省（字段不出现）⇒ 不改
/// - `null` ⇒ 清空回「不限」
/// - `200` ⇒ 改上限
///
/// 「清空」必须能与「不动」区分，否则「取消上限并保存」会被当成没改而静默丢失。
#[tokio::test]
async fn update_capacity_supports_three_states() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/shelves",
            Some(json!({
                "code": "S-CAP-02",
                "name": "Capacity-02",
                "zone": "PRODUCTION",
                "capacity": 100,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "create shelf: {env1}");
    let shelf_id: i64 = env1["data"]["id"].as_str().unwrap().parse().unwrap();

    async fn put(
        app: axum::Router,
        token: &str,
        id: i64,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        send(
            app,
            json_request(
                "POST",
                &format!("/shelves/{id}/update"),
                Some(body),
                Some(token),
            ),
        )
        .await
    }

    // ① 缺省 ⇒ 不改
    let (sa, enva) = put(app.clone(), &token, shelf_id, json!({ "version": 0 })).await;
    assert_eq!(sa, StatusCode::OK, "update: {enva}");
    assert_eq!(
        enva["data"]["capacity"].as_i64(),
        Some(100),
        "缺省 capacity 不应改动; got: {enva}"
    );

    // ② 改值
    let (sb, envb) = put(
        app.clone(),
        &token,
        shelf_id,
        json!({ "version": enva["data"]["version"].as_i64().unwrap(), "capacity": 200 }),
    )
    .await;
    assert_eq!(sb, StatusCode::OK, "update capacity: {envb}");
    assert_eq!(envb["data"]["capacity"].as_i64(), Some(200));

    // ③ 清空
    let (sc, envc) = put(
        app.clone(),
        &token,
        shelf_id,
        json!({ "version": envb["data"]["version"].as_i64().unwrap(), "capacity": null }),
    )
    .await;
    assert_eq!(sc, StatusCode::OK, "clear capacity: {envc}");
    assert_eq!(
        envc["data"]["capacity"].as_i64(),
        None,
        "null 必须清空成「不限」; got: {envc}"
    );

    // ④ `<= 0` 被接受（口径：<= 0 = 不限），不报 20104
    let (sd, envd) = send(
        app.clone(),
        json_request(
            "POST",
            "/shelves",
            Some(json!({
                "code": "S-CAP-03",
                "name": "Capacity-03",
                "zone": "PRODUCTION",
                "capacity": 0,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        sd,
        StatusCode::CREATED,
        "capacity=0 必须被接受（<= 0 = 不限），不报错; got: {envd}"
    );
    let _ = pool;
}
