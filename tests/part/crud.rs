//! part 域 CRUD + lifecycle 端到端集成测试 (Phase PR-CRUD)
//!
//! 覆盖 12 个新端点 + 4 个 lifecycle 流转：
//!   - list / detail / create / batch_create / update / soft_delete
//!   - upload-drawing (multipart 单独 #[ignore])
//!   - by-serial / deliver / cancel / complete / start-repair
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//! 每个用例按需使用 MANAGER / CLERK / INSPECTOR token。

// 2026-09-23 PR13 Phase C：edition 2024 下 `mod common;` 不自动 fallback 到 crate root。
// 2026-09-23 PR13 Phase G：helpers.rs 改为 thin barrel；fixture 由 PartFixture 提供。
#[path = "helpers.rs"]
mod helpers;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::part::service::PartService;
use hsh_erp_rust::modules::part_file::policy;
use hsh_erp_rust::modules::part_file::repo::PartFileRepo;
use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::*;

// ===========================================================================
//  动态 part/batch 插入 helper（sub-file 私有，PR-C 末统一迁）
// ===========================================================================

async fn insert_part(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    status: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-001', $4, $7, $3, $5, $5, 1, 0, $6, $6)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

async fn insert_batch(pool: &PgPool, part_id: i64, batch_no: i32, qty: i32, status: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(qty)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
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

async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_clerk() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Tests — Part 1: list / detail / by-serial 查询
// ===========================================================================

/// GET /parts?limit=10 —— 空库返回 200 / items=[] / total=0。
#[tokio::test]
async fn list_parts_basic() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts?limit=10", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list basic: {env}");
    assert_eq!(env["code"], 0);
    assert!(env["data"]["items"].is_array());
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 0);
    assert_eq!(env["data"]["total"], "0");
}

/// GET /parts?customer_id=&status=PENDING —— 3 PENDING + 1 INSPECTION，
/// filter PENDING 拿到 3 件 (L2 展开 + status 过滤)。
#[tokio::test]
async fn list_parts_filter_status_and_customer() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    use hsh_erp_rust::infra::clock::now_naive;
    let now = now_naive();
    let today = now.date();

    // 共用一个雪花生成器连发 4 个唯一 id
    for i in 0..3 {
        let id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
             applicant_name, request_date, planned_delivery_date, quantity, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $3, 'D-001', $4, 'PENDING', $3, $6, $6, 1, 0, $5, $5)",
        )
        .bind(id)
        .bind(format!("P{i:03}"))
        .bind(format!("P{i}"))
        .bind(fx.customer_l2_id)
        .bind(now)
        .bind(today)
        .execute(&pool)
        .await
        .expect("insert PENDING part");
    }
    let id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, 'PINS', 'PINSP', 'D-001', $2, 'INSPECTION', 'PINSP', $3, $3, 1, 0, $4, $4)",
    )
    .bind(id)
    .bind(fx.customer_l2_id)
    .bind(today)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert INSPECTION part");

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&status=PENDING", fx.customer_l2_id),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "filter status: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], "3",
        "应 PENDING×3 (1 个 INSPECTION 被过滤掉): {env}"
    );
    for item in env["data"]["items"].as_array().unwrap() {
        assert_eq!(item["status"], "PENDING");
    }
}

/// GET /parts?limit=2&offset=2 —— 5 件，offset=2 拿第 3、4 件（按默认 id DESC）。
#[tokio::test]
async fn list_parts_pagination_limit_offset() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    use hsh_erp_rust::infra::clock::now_naive;
    let now = now_naive();
    let today = now.date();
    let mut pids = Vec::new();
    for i in 0..5 {
        let id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
             applicant_name, request_date, planned_delivery_date, quantity, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $3, 'D-001', $4, 'PENDING', $3, $6, $6, 1, 0, $5, $5)",
        )
        .bind(id)
        .bind(format!("P{i:03}"))
        .bind(format!("P{i}"))
        .bind(fx.customer_l2_id)
        .bind(now)
        .bind(today)
        .execute(&pool)
        .await
        .expect("insert part");
        pids.push(id);
    }

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={}&limit=2&offset=2", fx.customer_l2_id),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "pagination: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["total"], "5", "总 5 件: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "limit=2 应返回 2 件: {env}");
    let mut pids_sorted = pids.clone();
    pids_sorted.sort_by(|a, b| b.cmp(a));
    let returned: Vec<i64> = items
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(
        returned,
        vec![pids_sorted[2], pids_sorted[3]],
        "offset=2 应返回 pids[4..6] 按 id DESC: {env}"
    );
}

/// GET /parts/{id} —— 标准详情返回 200 / status=PENDING / customer_name 冗余。
#[tokio::test]
async fn get_part_detail_200() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    let (s, env) = send(
        app,
        json_request("GET", &format!("/parts/{pid}"), None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "detail: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["id"], pid.to_string());
    assert_eq!(env["data"]["status"], "PENDING");
    assert_eq!(env["data"]["customer_name"], "FX 客户 L2");
    assert_eq!(env["data"]["l1_customer_name"], "FX 客户 L1");
}

/// GET /parts/{nonexistent_id} —— 20101 BIZ_PART_NOT_FOUND（HTTP 404）。
#[tokio::test]
async fn get_part_detail_404_not_found() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts/999999999", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "404: {env}");
    assert_eq!(env["code"], 20101, "BIZ_PART_NOT_FOUND: {env}");
}

/// GET /parts/{id} —— 软删后 GET → 20101 (get_part_detail 不含软删件)。
#[tokio::test]
async fn get_part_detail_404_soft_deleted() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    sqlx::query("UPDATE t_part SET deleted_at = now(), version = version + 1 WHERE id = $1")
        .bind(pid)
        .execute(&pool)
        .await
        .unwrap();

    let (s, env) = send(
        app,
        json_request("GET", &format!("/parts/{pid}"), None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "soft-deleted detail: {env}");
    assert_eq!(env["code"], 20101, "BIZ_PART_NOT_FOUND: {env}");
}

// ===========================================================================
//  Tests — Part 2: create / batch_create
// ===========================================================================

/// POST /parts —— MANAGER 创建成功：201 / status=PENDING / 新 id。
#[tokio::test]
async fn create_part_200_manager() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts",
            Some(json!({
                "name": "test part",
                "drawing_no": "D-TEST",
                "applicant_name": "张三",
                "quantity": 5,
                "request_date": today,
                "planned_delivery_date": today,
                "is_urgent": false,
                "customer_id": fx.customer_l2_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create 201: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["name"], "test part");
    assert_eq!(env["data"]["status"], "PENDING");
    assert_eq!(env["data"]["customer_id"], fx.customer_l2_id.to_string());
    assert!(!env["data"]["id"].as_str().unwrap().is_empty());
}

/// POST /parts —— INSPECTOR 角色 → 40300 FORBIDDEN。
#[tokio::test]
async fn create_part_403_inspector() {
    let (_pool, app, token, fx) = bootstrap_as_inspector().await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts",
            Some(json!({
                "name": "test part",
                "drawing_no": "D-TEST",
                "applicant_name": "张三",
                "quantity": 5,
                "request_date": today,
                "planned_delivery_date": today,
                "customer_id": fx.customer_l2_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "inspector 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// POST /parts —— 空 name → service 层 40001 VALIDATION_ERROR（HTTP 422）。
#[tokio::test]
async fn create_part_validation_failed() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts",
            Some(json!({
                "name": "",
                "drawing_no": "D-TEST",
                "applicant_name": "张三",
                "quantity": 5,
                "request_date": today,
                "planned_delivery_date": today,
                "customer_id": fx.customer_l2_id.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "empty name 422: {env}");
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
}

/// POST /parts/batch —— 2 件都成功 → 200 / created=2 / failed=[]。
#[tokio::test]
async fn batch_create_parts_all_success() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch",
            Some(json!({
                "customer_id": fx.customer_l2_id.to_string(),
                "items": [
                    {
                        "name": "batch-A",
                        "drawing_no": "D-A",
                        "applicant_name": "甲",
                        "quantity": 1,
                        "request_date": today,
                        "planned_delivery_date": today,
                    },
                    {
                        "name": "batch-B",
                        "drawing_no": "D-B",
                        "applicant_name": "乙",
                        "quantity": 1,
                        "request_date": today,
                        "planned_delivery_date": today,
                    },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "batch all success: {env}");
    assert_eq!(env["code"], 0);
    let created = env["data"]["created"].as_array().unwrap();
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(created.len(), 2, "应 created=2: {env}");
    assert_eq!(failed.len(), 0, "应 failed=0: {env}");
}

/// POST /parts/batch —— item1 正常 + item2 applicant_name 超长 (DB 拒绝)
/// → 1 created / 1 failed (50001 DATABASE)。
#[tokio::test]
async fn batch_create_parts_partial_failure() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let long_name = "甲".repeat(60);
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch",
            Some(json!({
                "customer_id": fx.customer_l2_id.to_string(),
                "items": [
                    {
                        "name": "ok-item",
                        "drawing_no": "D-OK",
                        "applicant_name": "甲",
                        "quantity": 1,
                        "request_date": today,
                        "planned_delivery_date": today,
                    },
                    {
                        "name": "bad-item",
                        "drawing_no": "D-BAD",
                        "applicant_name": long_name,
                        "quantity": 1,
                        "request_date": today,
                        "planned_delivery_date": today,
                    },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "batch partial: {env}");
    assert_eq!(env["code"], 0);
    let created = env["data"]["created"].as_array().unwrap();
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(created.len(), 1, "应 created=1: {env}");
    assert_eq!(failed.len(), 1, "应 failed=1: {env}");
    assert_eq!(failed[0]["code"], 50001, "DATABASE 兜底码: {env}");
    assert_eq!(failed[0]["item_index"], 1);
}

/// POST /parts/batch —— `[bad, ok, ok]` 三件：第 1 件失败，后续 2 件仍能成功。
#[tokio::test]
async fn batch_create_parts_savepoint_recovers_after_failure() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let long_name = "甲".repeat(60);
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch",
            Some(json!({
                "customer_id": fx.customer_l2_id.to_string(),
                "items": [
                    {
                        "name": "bad-item",
                        "drawing_no": "D-BAD",
                        "applicant_name": long_name,
                        "quantity": 1,
                        "request_date": today,
                        "planned_delivery_date": today,
                    },
                    {
                        "name": "ok-item-A",
                        "drawing_no": "D-OK-A",
                        "applicant_name": "甲",
                        "quantity": 1,
                        "request_date": today,
                        "planned_delivery_date": today,
                    },
                    {
                        "name": "ok-item-B",
                        "drawing_no": "D-OK-B",
                        "applicant_name": "乙",
                        "quantity": 1,
                        "request_date": today,
                        "planned_delivery_date": today,
                    },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "batch [bad, ok, ok]: {env}");
    assert_eq!(env["code"], 0);
    let created = env["data"]["created"].as_array().unwrap();
    let failed = env["data"]["failed"].as_array().unwrap();
    assert_eq!(created.len(), 2, "应 created=2: {env}");
    assert_eq!(failed.len(), 1, "应 failed=1: {env}");
    assert_eq!(failed[0]["item_index"], 0, "失败项应位于 idx=0: {env}");
    assert_eq!(failed[0]["code"], 50001, "DATABASE 兜底码（22001）: {env}");
}

// ===========================================================================
//  Tests — Part 3: update / soft-delete
// ===========================================================================

/// POST /parts/{id}/update —— 改 name + is_urgent → 200 / name 更新 / version 自增。
#[tokio::test]
async fn update_part_200() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/update"),
            Some(json!({
                "version": 0,
                "name": "renamed",
                "is_urgent": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "update 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["name"], "renamed");
    assert_eq!(env["data"]["is_urgent"], true);
    assert_eq!(env["data"]["version"], 1, "version 应自增 0→1: {env}");
}

/// POST /parts/{id}/update —— version 不匹配 → 40901 VERSION_CONFLICT (HTTP 409)。
#[tokio::test]
async fn update_part_version_conflict() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/update"),
            Some(json!({
                "version": 99,
                "name": "should-fail",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "version conflict: {env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");
}

/// POST /parts/{id}/update —— 已软删件 → 40901 VERSION_CONFLICT。
#[tokio::test]
async fn update_part_404_soft_deleted() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    sqlx::query("UPDATE t_part SET deleted_at = now(), version = version + 1 WHERE id = $1")
        .bind(pid)
        .execute(&pool)
        .await
        .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/update"),
            Some(json!({
                "version": 0,
                "name": "after-delete",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "update after delete: {env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");
}

/// POST /parts/{id}/soft-delete —— MANAGER 成功 → 200 / R.ok (data=null)。
#[tokio::test]
async fn soft_delete_part_manager_ok() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/soft-delete"),
            Some(json!({ "version": 0 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "soft delete 200: {env}");
    assert_eq!(env["code"], 0);

    let row: Option<(Option<chrono::NaiveDateTime>,)> =
        sqlx::query_as("SELECT deleted_at FROM t_part WHERE id = $1")
            .bind(pid)
            .fetch_optional(&pool)
            .await
            .unwrap();
    let deleted_at = row.unwrap().0;
    assert!(
        deleted_at.is_some(),
        "deleted_at 应被设置 (实际 None 表示未删)"
    );
}

/// POST /parts/{id}/soft-delete —— CLERK 角色 → 40300 FORBIDDEN。
#[tokio::test]
async fn soft_delete_part_403_clerk() {
    let (pool, app, token, fx) = bootstrap_as_clerk().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/soft-delete"),
            Some(json!({ "version": 0 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "clerk 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// POST /parts/{id}/soft-delete —— 重复软删同一件 → 20101 BIZ_PART_NOT_FOUND (404)。
#[tokio::test]
async fn soft_delete_part_404_soft_deleted() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    sqlx::query("UPDATE t_part SET deleted_at = now(), version = version + 1 WHERE id = $1")
        .bind(pid)
        .execute(&pool)
        .await
        .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/soft-delete"),
            Some(json!({ "version": 1 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "already deleted: {env}");
    assert_eq!(env["code"], 20101, "BIZ_PART_NOT_FOUND (已软删): {env}");
}

/// POST /parts/{id}/soft-delete —— PENDING + version 不匹配 → 40901。
#[tokio::test]
async fn soft_delete_part_409_version_conflict() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/soft-delete"),
            Some(json!({ "version": 99 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "version conflict: {env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");
}

/// POST /parts/{id}/soft-delete —— DELIVERED 终态 → 20119 BIZ_PART_NOT_DELETABLE。
#[tokio::test]
async fn soft_delete_part_409_terminal_status() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "DELIVERED",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/soft-delete"),
            Some(json!({ "version": 0 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "terminal status: {env}");
    assert_eq!(
        env["code"], 20119,
        "BIZ_PART_NOT_DELETABLE (终态禁删): {env}"
    );
}

// ===========================================================================
//  Tests — Part 4: by-serial
// ===========================================================================

/// GET /parts/by-serial/{serial} —— 命中 → 200 / id 一致。
#[tokio::test]
async fn get_by_serial_200() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("T-LOC-1"),
        "PENDING",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/parts/by-serial/T-LOC-1",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "by-serial hit: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["id"], pid.to_string());
}

/// GET /parts/by-serial/{serial} —— 找不到 → 20101 BIZ_PART_NOT_FOUND。
#[tokio::test]
async fn get_by_serial_404() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/parts/by-serial/NOT-EXIST",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "by-serial miss: {env}");
    assert_eq!(env["code"], 20101, "BIZ_PART_NOT_FOUND: {env}");
}

// ===========================================================================
//  Tests — Part 5: lifecycle (deliver / cancel / complete / start-repair)
// ===========================================================================

/// POST /parts/{id}/deliver —— READY_TO_SHIP → DELIVERED (200 + status)。
#[tokio::test]
async fn deliver_ready_to_ship_200() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "READY_TO_SHIP",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "READY_TO_SHIP").await;
    let bver = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": bver,
                "note": "发货"
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "deliver 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "DELIVERED");
}

/// POST /parts/{id}/deliver —— batch 当前 INSPECTION → 20117。
#[tokio::test]
async fn deliver_wrong_state_400() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
    let bver = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": bver,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "deliver wrong state: {env}");
    assert_eq!(env["code"], 20117, "BIZ_PART_NOT_READY_TO_SHIP: {env}");
}

/// POST /parts/{id}/cancel —— PENDING → CANCELLED (200 + status)。
#[tokio::test]
async fn cancel_pending_200() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/cancel"),
            Some(json!({ "reason": "客户取消" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "CANCELLED");
}

/// POST /parts/{id}/cancel —— COMPLETED 状态不能 cancel → 20103。
#[tokio::test]
async fn cancel_wrong_state_400() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        None,
        "COMPLETED",
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/cancel"),
            Some(json!({ "reason": "no-op" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "cancel completed: {env}");
    assert_eq!(env["code"], 20103, "BIZ_INVALID_TRANSITION: {env}");
}

/// POST /parts/{id}/complete —— DELIVERED → COMPLETED (200 + status)。
#[tokio::test]
async fn complete_delivered_200() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "DELIVERED",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "DELIVERED").await;
    let bver = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/complete"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": bver,
                "note": "归档"
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "COMPLETED");
    assert!(
        env["data"]["serial_no"].is_null(),
        "COMPLETED 应清空 serial_no: {env}"
    );
}

/// POST /parts/{id}/complete —— batch 当前 INSPECTION → 20116。
#[tokio::test]
async fn complete_wrong_state_400() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
    let bver = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/complete"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": bver,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "complete wrong state: {env}");
    assert_eq!(env["code"], 20116, "BIZ_PART_NOT_DELIVERED: {env}");
}

/// POST /parts/{id}/start-repair —— batch IN_PROCESS → REPAIRING (200 + status)。
#[tokio::test]
async fn start_repair_in_process_200() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "IN_PROCESS",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "IN_PROCESS").await;
    let bver = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/start-repair"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": bver,
                "reason": "尺寸偏大"
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start-repair 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "REPAIRING");
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_event \
         WHERE part_id = $1 AND event_type = 'REPAIR_STARTED'",
    )
    .bind(pid)
    .fetch_one(&pool)
    .await
    .expect("count REPAIR_STARTED events");
    assert!(
        event_count >= 1,
        "start-repair 应写 1 条 REPAIR_STARTED 事件日志到 t_part_event"
    );
}

/// POST /parts/{id}/start-repair —— batch PENDING → 20118。
#[tokio::test]
async fn start_repair_wrong_state_400() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "PENDING").await;
    let bver = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/start-repair"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": bver,
                "reason": "no-op"
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "start-repair wrong state: {env}"
    );
    assert_eq!(env["code"], 20118, "BIZ_PART_REPAIR_NOT_TRIGGERED: {env}");
}

/// POST /parts/{id}/deliver —— CANCELLED 状态 → 20115 BIZ_PART_ALREADY_CANCELLED (409)。
#[tokio::test]
async fn deliver_cancelled_409() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "CANCELLED",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "READY_TO_SHIP").await;
    let bver = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": bver,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "deliver cancelled: {env}");
    assert_eq!(env["code"], 20115, "BIZ_PART_ALREADY_CANCELLED: {env}");
}

// ===========================================================================
//  Tests — Part 7 (Fix Batch 2): lifecycle↔batch sync (Finding A)
//                       + cancel delivery_note lock (Finding D)
// ===========================================================================

/// 取 part 的某状态批次的 status 字符串 + version（验证 batch 同步用）。
async fn batch_status_and_version(
    pool: &PgPool,
    part_id: i64,
    status: &str,
) -> Option<(String, i32)> {
    let row: Option<(String, i32)> = sqlx::query_as(
        "SELECT status, version FROM t_part_batch \
         WHERE part_id = $1 AND status = $2 AND deleted_at IS NULL \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(part_id)
    .bind(status)
    .fetch_optional(pool)
    .await
    .unwrap();
    row
}

/// POST /parts/{id}/deliver —— READY_TO_SHIP → DELIVERED 应同时翻转
/// 指定 batch 到 DELIVERED（PR-B3 batch 级）。
#[tokio::test]
async fn deliver_also_updates_batch() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "READY_TO_SHIP",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "READY_TO_SHIP").await;
    let bver = batch_version(&pool, bid).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({
                "batch_id": bid.to_string(),
                "version": bver,
                "note": "发货"
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "deliver 200: {env}");
    assert_eq!(env["data"]["status"], "DELIVERED");

    let (batch_status, _) = batch_status_and_version(&pool, pid, "DELIVERED")
        .await
        .expect("批次应存在并已被翻为 DELIVERED");
    assert_eq!(
        batch_status, "DELIVERED",
        "batch.status 应同步翻为 DELIVERED"
    );
}

/// POST /parts/{id}/cancel —— PENDING → CANCELLED 应同时翻转最近一条
/// PENDING 批次到 CANCELLED（同事务）。
#[tokio::test]
async fn cancel_also_updates_batch() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;
    let _bid = insert_batch(&pool, pid, 1, 1, "PENDING").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/cancel"),
            Some(json!({ "reason": "客户取消" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel 200: {env}");
    assert_eq!(env["data"]["status"], "CANCELLED");

    let (batch_status, _) = batch_status_and_version(&pool, pid, "CANCELLED")
        .await
        .expect("批次应存在并已被翻为 CANCELLED");
    assert_eq!(
        batch_status, "CANCELLED",
        "batch.status 应同步翻为 CANCELLED"
    );
}

/// POST /parts/{id}/deliver —— batch_id 不存在 → 20109 BIZ_PART_BATCH_NOT_FOUND。
#[tokio::test]
async fn deliver_without_source_batch_409() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "READY_TO_SHIP",
    )
    .await;
    let fake_bid: i64 = 9_999_999_999;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({
                "batch_id": fake_bid.to_string(),
                "version": 0,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "deliver w/o batch: {env}");
    assert_eq!(env["code"], 20109, "BIZ_PART_BATCH_NOT_FOUND: {env}");
}

/// POST /parts/{id}/cancel —— part 已挂送货单 → 21420 BIZ_DELIVERY_NOTE_LOCKED_PART。
#[tokio::test]
async fn cancel_delivery_note_locked_409() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let pid = insert_part(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
    )
    .await;
    let _bid = insert_batch(&pool, pid, 1, 1, "PENDING").await;

    sqlx::query!(
        "UPDATE t_part_batch SET delivery_note_id = 88888888 \
         WHERE part_id = $1 AND deleted_at IS NULL",
        pid
    )
    .execute(&pool)
    .await
    .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/cancel"),
            Some(json!({ "reason": "测试锁定" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "delivery_note 锁 cancel: {env}");
    assert_eq!(env["code"], 21420, "BIZ_DELIVERY_NOTE_LOCKED_PART: {env}");

    let row: (String,) = sqlx::query_as("SELECT status FROM t_part WHERE id = $1")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "PENDING", "锁定 part 不应被 cancel");
}

// ===========================================================================
//  Tests — Part 6: upload-drawing (multipart, #[ignore] for this round)
// ===========================================================================

#[tokio::test]
#[ignore = "multipart body 构造需 reqwest client，本轮占位"]
async fn upload_drawing_integration() {
    // 占位
}

// ===========================================================================
//  Tests — Part 6.5: 上传 service 层（2026-09-11 新增）
// ===========================================================================

/// 构造 Manager 角色 CurrentUser（service 层直调需要 current 参数）
fn manager_current(id: i64) -> hsh_erp_rust::auth::rbac::CurrentUser {
    use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
    CurrentUser {
        id,
        username: "test-manager".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

/// 构造一个简单 PDF 字节（无需真实 PDF 格式，仅用于占位 bytes；service 不校验内容）
fn fake_pdf_bytes() -> Vec<u8> {
    b"%PDF-1.4\n%fake test drawing for upload integration\n%%EOF\n".to_vec()
}

/// 构造一个简单 STEP 字节
fn fake_step_bytes() -> Vec<u8> {
    b"ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\nENDSEC;\nEND-ISO-10303-21;\n".to_vec()
}

#[tokio::test]
#[ignore]
async fn upload_drawing_service_integration() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    let state = test_state_with_disabled_session(pool.clone());
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 99); // 99 区分测试进程

    // fixture 不预置 part（避免 PENDING 状态污染 list_parts_basic 等期望空库测试）；
    // 这里动态插一个 PENDING part 给 service upload 用。
    let part_id = insert_part(&pool, "P-DRAW", fx.customer_l2_id, None, "PENDING").await;

    let bytes = fake_pdf_bytes();
    let mut conn = state.pool.begin().await.unwrap();
    let pf = PartService::upload_drawing(
        &mut *conn,
        &snowflake,
        &state,
        part_id,
        &bytes,
        "drawing.pdf",
        "application/pdf",
        &manager_current(1),
    )
    .await
    .expect("upload_drawing 应成功");
    conn.commit().await.unwrap();

    assert_eq!(pf.kind, "DRAWING");
    assert_eq!(pf.file_type, "PDF");
    assert_eq!(pf.part_id, part_id);
    assert_eq!(pf.original_filename, "drawing.pdf");
    assert_eq!(pf.file_size, bytes.len() as i64);
    assert_eq!(pf.content_type, "application/pdf");
    assert_eq!(pf.upload_status, "READY");
    assert!(pf.content_sha256.is_some());
    assert!(
        pf.object_key
            .starts_with(&format!("uploads/part/{part_id}/DRAWING/")),
        "CAS key 格式不符: {}",
        pf.object_key
    );
    assert!(pf.object_key.ends_with("_drawing.pdf"));

    let row = PartFileRepo::get_by_part_kind(&pool, part_id, "DRAWING")
        .await
        .unwrap()
        .expect("DB 行应存在");
    assert_eq!(row.id, pf.id);
}

#[tokio::test]
#[ignore]
async fn upload_3d_model_service_integration() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    let state = test_state_with_disabled_session(pool.clone());
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 99);

    let part_id = insert_part(&pool, "P-3D", fx.customer_l2_id, None, "PENDING").await;

    let bytes = fake_step_bytes();
    let mut conn = state.pool.begin().await.unwrap();
    let pf = PartService::upload_3d_model(
        &mut *conn,
        &snowflake,
        &state,
        part_id,
        &bytes,
        "bracket.step",
        "application/step",
        &manager_current(1),
    )
    .await
    .expect("upload_3d_model 应成功");
    conn.commit().await.unwrap();

    assert_eq!(pf.kind, "3D_MODEL");
    assert_eq!(pf.file_type, "STEP");
    assert_eq!(pf.part_id, part_id);
    assert_eq!(pf.original_filename, "bracket.step");
    assert_eq!(pf.file_size, bytes.len() as i64);
    assert!(
        pf.object_key
            .starts_with(&format!("uploads/part/{part_id}/3D_MODEL/"))
    );
    assert!(pf.object_key.ends_with("_bracket.step"));

    let row = PartFileRepo::get_by_part_kind(&pool, part_id, "3D_MODEL")
        .await
        .unwrap()
        .expect("DB 行应存在");
    assert_eq!(row.id, pf.id);
    assert_eq!(row.file_type, "STEP");
}

#[tokio::test]
#[ignore]
async fn upload_bad_extension_rejected() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let state = test_state_with_disabled_session(pool.clone());
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 99);
    let part_id = insert_part(&pool, "P-BAD", fx.customer_l2_id, None, "PENDING").await;

    let mut conn = state.pool.begin().await.unwrap();
    let err = PartService::upload_drawing(
        &mut *conn,
        &snowflake,
        &state,
        part_id,
        b"junk".to_vec().as_slice(),
        "evil.step",
        "application/step",
        &manager_current(1),
    )
    .await
    .expect_err("应被扩展名白名单拦截");
    conn.rollback().await.unwrap();

    assert_eq!(
        err.code(),
        code::BIZ_PART_FILE_BAD_TYPE,
        "实际错误: {err:?}"
    );
}

#[tokio::test]
#[ignore]
async fn upload_content_type_mismatch_rejected() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let state = test_state_with_disabled_session(pool.clone());
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 99);
    let part_id = insert_part(&pool, "P-CT", fx.customer_l2_id, None, "PENDING").await;

    let mut conn = state.pool.begin().await.unwrap();
    let err = PartService::upload_drawing(
        &mut *conn,
        &snowflake,
        &state,
        part_id,
        b"junk".to_vec().as_slice(),
        "drawing.pdf",
        "image/png",
        &manager_current(1),
    )
    .await
    .expect_err("应被 content_type 校验拦截");
    conn.rollback().await.unwrap();

    assert_eq!(err.code(), code::BIZ_PART_FILE_BAD_TYPE);
}

// ===========================================================================
//  2026-09-16 M2-B review 第 2 轮 B1 修：cleanup_tmp_keys 全量收集回归测试
// ===========================================================================

#[tokio::test]
async fn batch_create_with_bindings_partial_failure_cleans_all_tmp() {
    use std::sync::Arc;
    use hsh_erp_rust::modules::part::dto_crud::{
        FileBindingIn, PartBatchCreateItem, PartBatchCreateRequest,
    };

    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let current = manager_current(1);
    let cos = std::sync::Arc::new(MockCos::new());

    let tmp_key_0 = "tmp/test/binding-clean-0.pdf";
    let tmp_key_1 = "tmp/test/binding-clean-1.pdf";
    let sha_0 = "0".repeat(64);
    let sha_1 = "1".repeat(64);
    cos.set_head(tmp_key_0, 1024);
    cos.set_head(tmp_key_1, 1024);

    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let long_name = "甲".repeat(60);
    let req = PartBatchCreateRequest {
        customer_id: fx.customer_l2_id,
        items: vec![
            PartBatchCreateItem {
                name: "bad-item".into(),
                drawing_no: "D-BAD".into(),
                applicant_name: long_name,
                quantity: 1,
                request_date: today,
                planned_delivery_date: today,
                is_urgent: false,
                order_no: None,
                system_delivery_date: None,
                note: None,
                assembly_id: None,
                drawing_file: Some(FileBindingIn {
                    tmp_key: tmp_key_0.into(),
                    content_sha256: sha_0.clone(),
                    original_filename: "first.pdf".into(),
                    file_size: 1024,
                    content_type: "application/pdf".into(),
                }),
                model3d_file: None,
            },
            PartBatchCreateItem {
                name: "ok-item".into(),
                drawing_no: "D-OK".into(),
                applicant_name: "乙".into(),
                quantity: 1,
                request_date: today,
                planned_delivery_date: today,
                is_urgent: false,
                order_no: None,
                system_delivery_date: None,
                note: None,
                assembly_id: None,
                drawing_file: Some(FileBindingIn {
                    tmp_key: tmp_key_1.into(),
                    content_sha256: sha_1.clone(),
                    original_filename: "second.pdf".into(),
                    file_size: 1024,
                    content_type: "application/pdf".into(),
                }),
                model3d_file: None,
            },
        ],
    };

    let mut tx = pool.begin().await.unwrap();
    let (out, keys) = PartService::batch_create_parts_with_bindings(
        &mut *tx,
        &snowflake,
        cos.clone(),
        "uploads",
        "tmp/",
        &req,
        &current,
    )
    .await
    .map_err(|(e, _)| e)
    .expect("batch_create_parts_with_bindings 应整体返回 Ok（含 failed）");
    tx.commit().await.unwrap();

    assert_eq!(
        out.created.len(),
        1,
        "应 created=1: failed={:?}",
        out.failed
    );
    assert_eq!(out.failed.len(), 1, "应 failed=1");
    assert_eq!(out.failed[0].item_index, 0, "失败项位于 idx=0");
    assert_eq!(
        out.failed[0].code, 50001,
        "DATABASE 兜底码（applicant_name 超长 22001）"
    );

    assert_eq!(
        keys.len(),
        2,
        "cleanup_tmp_keys 应包含 2 个 key（成功 + 失败 item 各 1）: got {keys:?}"
    );
    assert!(
        keys.contains(&tmp_key_0.to_string()),
        "失败 item 的 tmp_key 必须保留在 cleanup_tmp_keys 中: {keys:?}"
    );
    assert!(
        keys.contains(&tmp_key_1.to_string()),
        "成功 item 的 tmp_key 也必须在 cleanup_tmp_keys 中: {keys:?}"
    );

    assert_eq!(cos.head_call_count(tmp_key_0), 1, "item 0 head 应被调 1 次");
    assert_eq!(cos.head_call_count(tmp_key_1), 1, "item 1 head 应被调 1 次");
    let copy_calls = cos.copy_calls.lock().unwrap().clone();
    assert_eq!(
        copy_calls.len(),
        2,
        "copy_object 应被调 2 次（每个 binding 一份）: got {copy_calls:?}"
    );

    assert_eq!(
        cos.delete_call_count(tmp_key_0),
        0,
        "service 层不应直接 spawn delete（属 handler 职责）"
    );
    let _ = Arc::new(()); // suppress unused
}

#[tokio::test]
#[ignore]
async fn policy_unit_smoke_integration() {
    assert_eq!(policy::allowed_exts("DRAWING"), &["pdf"]);
    assert!(policy::allowed_exts("3D_MODEL").contains(&"step"));
    assert!(policy::allowed_exts("3D_MODEL").contains(&"stl"));
    assert_eq!(policy::file_type_for_ext("pdf"), Some("PDF"));
    assert_eq!(policy::file_type_for_ext("STEP"), Some("STEP"));
    assert_eq!(policy::file_type_for_ext("step"), Some("STEP"));
    assert_eq!(policy::file_type_for_ext("unknown"), None);
    let ext = policy::ext_of("foo.PDF");
    assert_eq!(ext.as_deref(), Some("pdf"));
    assert_eq!(policy::ext_of("noext"), None);
}

// ===========================================================================
// 2026-09-16 M2-C：handler 后置 spawn delete 端到端断言
// ===========================================================================

#[tokio::test]
async fn batch_create_parts_handler_spawn_delete_on_ok() {
    use std::sync::Arc;

    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    let cos = Arc::new(MockCos::new());
    let tmp_key = "tmp/test/handler-spawn-delete.pdf".to_string();
    let sha = "a".repeat(64);
    cos.set_head(&tmp_key, 1024);

    let state = test_state_with_cos(pool.clone(), cos.clone()).await;
    let app = test_app(state.clone());

    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let req_body = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "items": [{
            "name": "spawn-delete-item",
            "drawing_no": "D-SPAWN",
            "applicant_name": "丙",
            "quantity": 1,
            "request_date": today.to_string(),
            "planned_delivery_date": today.to_string(),
            "is_urgent": false,
            "drawing_file": {
                "tmp_key": tmp_key,
                "content_sha256": sha,
                "original_filename": "handler.pdf",
                "file_size": "1024",
                "content_type": "application/pdf"
            },
            "model3d_file": null,
        }],
    });

    let (status, envelope) = send(
        app,
        json_request("POST", "/parts/batch", Some(req_body), Some(&token)),
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::OK, "应 200: {envelope}");
    assert_eq!(envelope["code"], 0, "应 code=0: {envelope}");
    let created = envelope["data"]["created"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(created, 1, "应 created=1: {envelope}");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let delete_calls = cos.delete_calls.lock().unwrap().clone();
    assert!(
        delete_calls
            .iter()
            .any(|k| k == "tmp/test/handler-spawn-delete.pdf"),
        "handler spawn delete 应触发 tmp_key 兜底: got {delete_calls:?}"
    );
}

#[tokio::test]
async fn batch_create_parts_handler_spawn_delete_on_err() {
    use std::sync::Arc;

    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    let cos = Arc::new(MockCos::new());
    let first_tmp_key = "tmp/test/handler-err-spawn-delete-first.pdf";
    let second_tmp_key = "tmp/test/handler-err-spawn-delete-second.pdf";
    let first_sha = "a".repeat(64);
    let second_sha = "b".repeat(64);
    cos.set_head(first_tmp_key, 1024);

    let state = test_state_with_cos(pool.clone(), cos.clone()).await;
    let app = test_app(state.clone());

    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let req_body = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "items": [
            {
                "name": "first-item",
                "drawing_no": "D-FIRST",
                "applicant_name": "甲",
                "quantity": 1,
                "request_date": today.to_string(),
                "planned_delivery_date": today.to_string(),
                "is_urgent": false,
                "drawing_file": {
                    "tmp_key": first_tmp_key,
                    "content_sha256": first_sha,
                    "original_filename": "first.pdf",
                    "file_size": "1024",
                    "content_type": "application/pdf"
                },
                "model3d_file": null,
            },
            {
                "name": "second-item",
                "drawing_no": "D-SECOND",
                "applicant_name": "乙",
                "quantity": 1,
                "request_date": today.to_string(),
                "planned_delivery_date": today.to_string(),
                "is_urgent": false,
                "drawing_file": {
                    "tmp_key": second_tmp_key,
                    "content_sha256": second_sha,
                    "original_filename": "second.pdf",
                    "file_size": "1024",
                    "content_type": "application/pdf"
                },
                "model3d_file": null,
            },
        ],
    });

    let (status, envelope) = send(
        app,
        json_request("POST", "/parts/batch", Some(req_body), Some(&token)),
    )
    .await;

    assert!(
        !status.is_success(),
        "Err 路径应非 2xx: status={status}, envelope={envelope}"
    );
    assert_eq!(
        envelope["code"], 21114,
        "Err 路径应报 21114 BIZ_PART_FILE_TMP_OBJECT_MISSING: {envelope}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let delete_calls = cos.delete_calls.lock().unwrap().clone();
    assert!(
        delete_calls.iter().any(|k| k == first_tmp_key),
        "Err 路径下 handler 应 spawn delete first_tmp_key 兜底: got {delete_calls:?}"
    );
    assert!(
        !delete_calls.iter().any(|k| k == second_tmp_key),
        "second_tmp_key 因 head 失败未进 COS，不应被 delete: got {delete_calls:?}"
    );
}