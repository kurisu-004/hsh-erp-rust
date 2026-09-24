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

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::{json_request, load_part_fixture, login_token, send, test_app,
    test_pool, test_state};

// ===========================================================================
//  动态 fixture helpers（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::insert_shelf` 引入，因 fixtures.rs
//  本轮被删，复制到本地（同形 sqlx::query 直插）。
// ===========================================================================

/// 插一个 t_shelf 行（code / name / zone）。
async fn insert_shelf(pool: &PgPool, code: &str, name: &str, zone: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_test_support::pool_snowflake;

    let snowflake = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(zone)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

// ===========================================================================
//  动态 part/batch 插入 helper（sub-file 私有，PR-C 末统一迁）
// ===========================================================================

async fn insert_part(pool: &PgPool, name: &str, customer_id: i64, status: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, NULL, $2, 'D-001', $3, $5, $2, $4, $4, 1, 0, $6, $6)",
    )
    .bind(part_id)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");
    part_id
}

/// 在 part 上追加一个非默认 status / location / holder 的批次。
async fn add_batch_with_location(
    pool: &PgPool,
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

// ===========================================================================
//  bootstrap helpers
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. 多 batch part（不同 status / location / holder）→ 返回 min-progress 批次的派生字段。
#[tokio::test]
async fn list_returns_min_progress_batch_location_and_holder() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let prod_shelf = insert_shelf(&pool, "PROD-LIST-001", "FX 检验架", "INSPECTION").await;

    // 1 part + 3 batches（part 状态由 rollup 规则决定，但此处不依赖：只看批次）
    let pid = insert_part(&pool, "P0", fx.customer_l2_id, "PENDING").await;
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

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&limit=10", fx.customer_l2_id),
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
#[tokio::test]
async fn list_returns_null_location_when_no_active_batches() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let _pid = insert_part(&_pool, "P0", fx.customer_l2_id, "PENDING").await;
    let _ = _pid;

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&limit=10", fx.customer_l2_id),
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
#[tokio::test]
async fn list_resolves_production_shelf_holder_to_shelf_code() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let prod_shelf = insert_shelf(&pool, "SHELF-CODE-XYZ", "FX 检验架", "INSPECTION").await;

    let pid = insert_part(&pool, "P0", fx.customer_l2_id, "IN_PROCESS").await;
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

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&limit=10", fx.customer_l2_id),
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

/// `GET /parts?locations=PRODUCTION_SHELF,WORKER` —— 仅返 part 下至少有一个
/// active batch.location 命中白名单的 part。OFFICE 批次的 part 不应出现。
#[tokio::test]
async fn list_filters_by_locations_param() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let prod_shelf = insert_shelf(&pool, "PROD-LOC-001", "FX 检验架", "INSPECTION").await;

    // part_a：唯一批次 location=OFFICE → 不应命中
    let pid_a = insert_part(&pool, "PA", fx.customer_l2_id, "PENDING").await;
    add_batch_with_location(&pool, pid_a, 1, 1, "PENDING", Some("OFFICE"), None).await;
    // part_b：唯一批次 location=PRODUCTION_SHELF → 应命中
    let pid_b = insert_part(&pool, "PB", fx.customer_l2_id, "IN_PROCESS").await;
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

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&locations=PRODUCTION_SHELF,WORKER&limit=10", fx.customer_l2_id),
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
#[tokio::test]
async fn list_filters_by_holder_ids_param_polymorphic() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let shelf_a = insert_shelf(&pool, "PROD-HOLDER-001", "FX 检验架A", "INSPECTION").await;
    let shelf_b = insert_shelf(&pool, "PROD-HOLDER-002", "FX 检验架B", "INSPECTION").await;

    // 建一个 worker
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
    let pid_x = insert_part(&pool, "PX", fx.customer_l2_id, "IN_PROCESS").await;
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
    let pid_y = insert_part(&pool, "PY", fx.customer_l2_id, "IN_PROCESS").await;
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
    let pid_z = insert_part(&pool, "PZ", fx.customer_l2_id, "IN_PROCESS").await;
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

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&holder_ids={},{}&limit=10", fx.customer_l2_id, shelf_a, worker_id),
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
#[tokio::test]
async fn list_without_locations_or_holder_ids_returns_all() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let prod_shelf = insert_shelf(&pool, "PROD-BASE-001", "FX 检验架", "INSPECTION").await;

    let pid_a = insert_part(&pool, "PALL-A", fx.customer_l2_id, "PENDING").await;
    add_batch_with_location(&pool, pid_a, 1, 1, "PENDING", Some("OFFICE"), None).await;
    let pid_b = insert_part(&pool, "PALL-B", fx.customer_l2_id, "IN_PROCESS").await;
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
    let pid_c = insert_part(&pool, "PALL-C", fx.customer_l2_id, "IN_PROCESS").await;
    add_batch_with_location(&pool, pid_c, 1, 1, "IN_PROCESS", Some("WORKER"), None).await;

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&limit=10", fx.customer_l2_id),
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
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&holder_ids=not_a_number", fx.customer_l2_id),
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