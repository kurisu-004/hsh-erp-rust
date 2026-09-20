//! 2026-09-16 PR-2（part-slim-down）回归测试 —— `GET /parts` 列表返回的
//! `location` / `holder_name` 派生自 min-progress 活跃批次。
//!
//! PR-2 § parts/dto_crud.rs `PartListItem` 新增 2 字段：
//! - `location`：min-progress 活跃批次的 location
//! - `holder_name`：同批次 current_holder_id 解析为 t_shelf.code / t_worker.name /
//!   t_outsource_company.name（按 batch.location 分桶）
//!
//! 实现：part/service/crud.rs `enrich_part_list_with_location_and_holder` helper，
//! 1 条 list_active_by_part_ids 拉活跃批次 + 3 条 IN 查询解析 holder 名称
//! （与页大小 N 无关）。
//!
//! 本测试覆盖：
//! 1. 多批次 part（不同 status / location / holder）→ 返回 min-progress 批次的派生字段
//! 2. 无活跃批次 part → location=null / holder_name=null

#[path = "common/mod.rs"]
mod common;

#[path = "part_api_helpers.rs"]
mod helpers;

use axum::http::StatusCode;
use serde_json::Value;

use helpers::*;

// ===========================================================================
//  全局串行化
// ===========================================================================


async fn setup() -> sqlx::PgPool {
    common::ensure_database_exists().await;
    let pool = common::test_pool().await;
    common::clean_db(&pool).await;
    common::clean_business_db(&pool).await;
    pool
}

/// 在 part 上追加一个非默认 status / location / holder 的批次。
///
/// 返回 `(batch_id, location, holder_id, holder_name_or_code)`，便于测试断言。
async fn add_batch_with_location(
    pool: &sqlx::PgPool,
    part_id: i64,
    batch_no: i32,
    qty: i32,
    status: &str,
    location: Option<&str>,
    holder_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    // 2026-09-16 PR-2（migration 027）：t_part_batch 删 has_been_repaired。
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, $8)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(qty)
    .bind(status)
    .bind(location)
    .bind(holder_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch with location/holder");
    id
}

/// 在 t_shelf 插一行；返回 shelf.id。
async fn insert_insp_shelf(pool: &sqlx::PgPool, code: &str) -> i64 {
    common::insert_shelf(pool, code, "品检架", "INSPECTION").await
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. 多 batch part（不同 status / location / holder）→ 返回 min-progress 批次的派生字段。
///
/// 派生规则（PR-2 § part/service/crud.rs::enrich_part_list_with_location_and_holder）：
///   - 排除 CANCELLED；非空时再排除 COMPLETED
///   - 取 part_status_progress 最小的批次
///   - location = 该批次 location；holder_name = 该批次 current_holder_id 解析名
///     （按 batch.location 分桶：SHELF → t_shelf.code；WORKER → t_worker.name）
///
/// 场景：part P0 3 个批次：
///   - batch 1: status=PENDING (progress=0) + location=OFFICE + holder=NULL → 应选中
///   - batch 2: status=IN_PROCESS (progress=2) + location=PRODUCTION_SHELF
///             + holder=insp_shelf
///   - batch 3: status=DELIVERED (progress=6) + location=PRODUCTION_SHELF
///             + holder=insp_shelf
///
/// 期望：list 返回 `location="OFFICE"`，`holder_name=null`（PENDING 批次无 holder）。
#[tokio::test]
async fn list_returns_min_progress_batch_location_and_holder() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let prod_shelf = insert_insp_shelf(&pool, "PROD-LIST-001").await;

    // 1 part + 3 batches（part 状态由 rollup 规则决定，但此处不依赖：只看批次）
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    // batch 1（最小 progress）
    add_batch_with_location(&pool, pid, 1, 1, "PENDING", Some("OFFICE"), None).await;
    // batch 2（IN_PROCESS + shelf holder）
    add_batch_with_location(
        &pool,
        pid,
        2,
        1,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(prod_shelf),
    )
    .await;
    // batch 3（DELIVERED + shelf holder）
    add_batch_with_location(
        &pool,
        pid,
        3,
        1,
        "DELIVERED",
        Some("PRODUCTION_SHELF"),
        Some(prod_shelf),
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&limit=10"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "应见 1 件 part: {env}");
    let item = &items[0];
    assert_eq!(
        item["location"], "OFFICE",
        "min-progress 批次 (PENDING) location 应=OFFICE: {item}"
    );
    assert!(
        item["holder_name"].is_null(),
        "PENDING 批次无 holder，holder_name 应为 null: {item}"
    );
}

/// 2. 无任何批次的 part（`list_active_by_part_ids` 返回空）→ location=null / holder_name=null。
///
/// 派生规则：`enrich_part_list_with_location_and_holder` 在 `list_active_by_part_ids`
/// 返回空时，`target_per_part` 为空；caller `list_parts` 走 `(None, None)` 默认值
///（PR-2 § part/service/crud.rs:247-249）。
///
/// 注：「全部批次 CANCELLED」并不触发空 —— service 层会把 CANCELLED 批次作为
/// fallback 候选（PR-2 § crud.rs:919-921 `bs.iter().collect()`），仍派生 location
///（值为该 CANCELLED 批次的 location）。要触发 location=null 必须让 part 真正
/// 没有非软删批次（即 `list_active_by_part_ids` 返回空）。
#[tokio::test]
async fn list_returns_null_location_when_no_active_batches() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    // 1 part + 0 批次（list_active_by_part_ids 返回空）
    let _pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&limit=10"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "应见 1 件 part: {env}");
    let item = &items[0];
    assert!(
        item["location"].is_null(),
        "无任何批次的 part → location 应为 null: {item}"
    );
    assert!(
        item["holder_name"].is_null(),
        "无任何批次的 part → holder_name 应为 null: {item}"
    );
}

/// 3. IN_PROCESS 批次 + PRODUCTION_SHELF holder → holder_name = shelf.code。
///
/// 直接验证 holder_name 解析路径（PRODUCTION_SHELF → t_shelf.code）。
#[tokio::test]
async fn list_resolves_production_shelf_holder_to_shelf_code() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let prod_shelf = insert_insp_shelf(&pool, "SHELF-CODE-XYZ").await;

    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "IN_PROCESS").await;
    add_batch_with_location(
        &pool,
        pid,
        1,
        1,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(prod_shelf),
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&limit=10"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "应见 1 件 part: {env}");
    let item = &items[0];
    assert_eq!(
        item["location"], "PRODUCTION_SHELF",
        "IN_PROCESS 批次 location 应保留: {item}"
    );
    assert_eq!(
        item["holder_name"], "SHELF-CODE-XYZ",
        "holder_name 应解析为 t_shelf.code: {item}"
    );
}

// ===== 2026-09-17 PR-4 守卫修复：locations / holder_ids 过滤 =====
//
// 背景：前端 `usePartsListQuery.ts:204` 发 `locations` + `holder_ids` 两个 query
// 参数，被 PartListQuery 静默忽略。本测试验证 PR-4 修复后两端点都按 t_part_batch
// （PR-2 已删 t_part.location / current_holder_id 列）过滤。

/// `GET /parts?locations=PRODUCTION_SHELF,WORKER` —— 仅返 part 下至少有一个
/// active batch.location 命中白名单的 part。OFFICE 批次的 part 不应出现。
#[tokio::test]
async fn list_filters_by_locations_param() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let prod_shelf = insert_insp_shelf(&pool, "PROD-LOC-001").await;

    // part_a：唯一批次 location=OFFICE → 不应命中
    let pid_a = insert_part_with_status(&pool, "PA", l2, None, None, "PENDING").await;
    add_batch_with_location(&pool, pid_a, 1, 1, "PENDING", Some("OFFICE"), None).await;
    // part_b：唯一批次 location=PRODUCTION_SHELF → 应命中
    let pid_b = insert_part_with_status(&pool, "PB", l2, None, None, "IN_PROCESS").await;
    add_batch_with_location(
        &pool,
        pid_b,
        1,
        1,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(prod_shelf),
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr_loc").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&locations=PRODUCTION_SHELF,WORKER&limit=10"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("items");
    let names: Vec<&str> = items
        .iter()
        .map(|i| i["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        names.contains(&"PB"),
        "PB（PRODUCTION_SHELF 批次）应在结果中: {env}"
    );
    assert!(
        !names.contains(&"PA"),
        "PA（OFFICE 批次）不应在结果中（被 locations 过滤掉）: {env}"
    );
}

/// `GET /parts?holder_ids=<shelf_id>` —— 多态 holder：t_shelf / t_worker /
/// t_outsource_company 任一表匹配同一雪花 id 即命中。
///
/// 场景：shelves 1 个（PROD-HOLDER-001）→ holder_id 命中其 id；worker 1 个
/// (W-001) → holder_id 命中其 id（模拟 worker 已接管该 part 批次）。
/// 构造 3 个 part：
/// - part_x：批次 1 holder=shelf_id → 命中
/// - part_y：批次 1 holder=worker_id → 命中（多态：worker 也命中）
/// - part_z：批次 1 holder=另一个 shelf → 不命中
#[tokio::test]
async fn list_filters_by_holder_ids_param_polymorphic() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let shelf_a = insert_insp_shelf(&pool, "PROD-HOLDER-001").await;
    let shelf_b = insert_insp_shelf(&pool, "PROD-HOLDER-002").await;

    // 建一个 worker（直插 t_worker，复用 common 已有 helper 模式）
    let worker_id = {
        use hsh_erp_rust::infra::clock::now_naive;
        use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
        let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
        let id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_worker (id, badge_code, name, work_type_id, is_active, version, \
             created_at, created_by, updated_at, updated_by) \
             VALUES ($1, $2, $3, NULL, true, 0, $4, NULL, $4, NULL)",
        )
        .bind(id)
        .bind("BADGE-W-001")
        .bind("W-001")
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert worker");
        id
    };

    // part_x：批次 holder=shelf_a（命中 shelf_a 的 id）
    let pid_x = insert_part_with_status(&pool, "PX", l2, None, None, "IN_PROCESS").await;
    add_batch_with_location(
        &pool,
        pid_x,
        1,
        1,
        "IN_PROCESS",
        Some("WORKER"),
        Some(shelf_a),
    )
    .await;
    // part_y：批次 holder=worker（命中 worker_id）
    let pid_y = insert_part_with_status(&pool, "PY", l2, None, None, "IN_PROCESS").await;
    add_batch_with_location(
        &pool,
        pid_y,
        1,
        1,
        "IN_PROCESS",
        Some("WORKER"),
        Some(worker_id),
    )
    .await;
    // part_z：批次 holder=shelf_b（不在 holder_ids 白名单）
    let pid_z = insert_part_with_status(&pool, "PZ", l2, None, None, "IN_PROCESS").await;
    add_batch_with_location(
        &pool,
        pid_z,
        1,
        1,
        "IN_PROCESS",
        Some("WORKER"),
        Some(shelf_b),
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr_holder").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&holder_ids={shelf_a},{worker_id}&limit=10"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("items");
    let names: Vec<&str> = items
        .iter()
        .map(|i| i["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        names.contains(&"PX"),
        "PX（shelf_a holder）应在结果中: {env}"
    );
    assert!(
        names.contains(&"PY"),
        "PY（worker holder）应在结果中（多态命中）: {env}"
    );
    assert!(
        !names.contains(&"PZ"),
        "PZ（shelf_b holder）不应在结果中: {env}"
    );
}

/// 不传 locations / holder_ids 时与旧行为一致（不过滤这两个维度）。
/// 场景：3 个 part 各有 OFFICE / PRODUCTION_SHELF / WORKER 三种批次，
/// 不传过滤 → 应全部返回。
#[tokio::test]
async fn list_without_locations_or_holder_ids_returns_all() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let prod_shelf = insert_insp_shelf(&pool, "PROD-BASE-001").await;

    let pid_a = insert_part_with_status(&pool, "PALL-A", l2, None, None, "PENDING").await;
    add_batch_with_location(&pool, pid_a, 1, 1, "PENDING", Some("OFFICE"), None).await;
    let pid_b = insert_part_with_status(&pool, "PALL-B", l2, None, None, "IN_PROCESS").await;
    add_batch_with_location(
        &pool,
        pid_b,
        1,
        1,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(prod_shelf),
    )
    .await;
    let pid_c = insert_part_with_status(&pool, "PALL-C", l2, None, None, "IN_PROCESS").await;
    add_batch_with_location(&pool, pid_c, 1, 1, "IN_PROCESS", Some("WORKER"), None).await;

    let (app, token, _pool) = login_manager(pool, "mgr_no_filt").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&limit=10"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("items");
    let names: Vec<&str> = items
        .iter()
        .map(|i| i["name"].as_str().unwrap_or(""))
        .collect();
    for expected in ["PALL-A", "PALL-B", "PALL-C"] {
        assert!(
            names.contains(&expected),
            "{expected} 应在结果中（不过滤）：{env}"
        );
    }
}

/// holder_ids 包含非法雪花 ID → 返回 40001 VALIDATION_ERROR（service 层 parse 失败兜底）。
#[tokio::test]
async fn list_holder_ids_invalid_format_returns_40001() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let (app, token, _pool) = login_manager(pool, "mgr_bad_h").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&holder_ids=not_a_number"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "非法 holder_ids 应 422: {env}"
    );
    assert_eq!(
        env["code"].as_i64().unwrap(),
        40001,
        "expected VALIDATION_ERROR: {env}"
    );
}
