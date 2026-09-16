//! part 域 CRUD + lifecycle 端到端集成测试 (Phase PR-CRUD)
//!
//! 覆盖 12 个新端点 + 4 个 lifecycle 流转：
//!   - list / detail / create / batch_create / update / soft_delete
//!   - upload-drawing (multipart 单独 #[ignore])
//!   - by-serial / deliver / cancel / complete / start-repair
//!
//! ## 并行 / 认证
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。
//! 每个用例按需使用 MANAGER / CLERK / INSPECTOR token。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{add_role, clean_business_db, clean_db, insert_user_with_password, test_app, test_pool, test_state, test_state_with_disabled_session};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::part::service::PartService;
use hsh_erp_rust::modules::part_file::policy;
use hsh_erp_rust::modules::part_file::repo::PartFileRepo;
use hsh_erp_rust::shared::error::code;

// ===========================================================================
//  全局串行化 + helpers (拷贝自 tests/part_api.rs，按约定不跨文件复用)
// ===========================================================================

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let envelope: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("parse JSON: {e}; raw = {}", String::from_utf8_lossy(&body)));
    (status, envelope)
}

fn json_request(method: &str, uri: &str, body: Option<Value>, bearer: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        builder = builder.header(AUTHORIZATION, format!("Bearer {t}"));
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    builder.body(body).expect("build request")
}

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

// ----- 角色登录 helper（拷贝自 tests/part_api.rs） -----

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": username, "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token, pool)
}

async fn login_inspector(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "INSPECTOR", None, None).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": username, "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token, pool)
}

async fn login_clerk(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "CLERK", None, None).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": username, "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token, pool)
}

// ----- Fixture helpers (拷贝自 tests/part_api.rs) -----

async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id, name, prefix, now,
    )
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id, name, l1_id, now,
    )
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

async fn insert_part_with_status(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    assembly_id: Option<i64>,
    status: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, \
         quantity, has_been_repaired, version, created_at, created_by, updated_at, updated_by, \
         assembly_id) \
         VALUES ($1, $2, $3, 'D-001', $4, $8, $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL, $7)",
        id, serial_no, name, customer_id, now, today, assembly_id, status,
    )
    .execute(pool)
    .await
    .expect("insert part");
    id
}

#[allow(dead_code)]
async fn insert_batch(pool: &PgPool, part_id: i64, batch_no: i32, qty: i32, status: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, has_been_repaired, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, false, 0, $6, NULL, $6, NULL)",
        id, part_id, batch_no, qty, status, now,
    )
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

// ===========================================================================
//  Tests — Part 1: list / detail / by-serial 查询
// ===========================================================================

/// GET /parts?limit=10 —— 空库返回 200 / items=[] / total=0。
#[tokio::test]
async fn list_parts_basic() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts?limit=10", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list basic: {env}");
    assert_eq!(env["code"], 0);
    assert!(env["data"]["items"].is_array());
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 0);
    // total 是 i64 经 serialize_i64 → JSON string（防 JS 精度截断）
    assert_eq!(env["data"]["total"], "0");
}

/// GET /parts?customer_id=&status=PENDING —— 3 PENDING + 1 INSPECTION，
/// filter PENDING 拿到 3 件 (L2 展开 + status 过滤)。
#[tokio::test]
async fn list_parts_filter_status_and_customer() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    // 共用一个雪花生成器连发 4 个唯一 id（避免 4 次独立 generator 在同一
    // 毫秒内拿到重复 id 触发 23505 pkey 冲突）。
    {
        let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
            1_577_836_800_000,
            1,
        );
        use hsh_erp_rust::infra::clock::now_naive;
        for i in 0..3 {
            let now = now_naive();
            let today = now.date();
            let id = snowflake.next_id();
            sqlx::query!(
                "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
                 applicant_name, request_date, planned_delivery_date, \
                 quantity, has_been_repaired, version, created_at, created_by, updated_at, updated_by, \
                 assembly_id) \
                 VALUES ($1, $2, $3, 'D-001', $4, $8, $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL, $7)",
                id,
                format!("P{i:03}"),
                format!("P{i}"),
                l2,
                now,
                today,
                None::<i64>,
                "PENDING",
            )
            .execute(&pool)
            .await
            .expect("insert PENDING part");
        }
        let now = now_naive();
        let today = now.date();
        let id = snowflake.next_id();
        sqlx::query!(
            "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
             applicant_name, request_date, planned_delivery_date, \
             quantity, has_been_repaired, version, created_at, created_by, updated_at, updated_by, \
             assembly_id) \
             VALUES ($1, $2, $3, 'D-001', $4, $8, $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL, $7)",
            id,
            "PINS",
            "PINSP",
            l2,
            now,
            today,
            None::<i64>,
            "INSPECTION",
        )
        .execute(&pool)
        .await
        .expect("insert INSPECTION part");
    }

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&status=PENDING"),
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    // 共用一个雪花生成器连发 5 个唯一 id（避免 5 次独立 generator 在同一
    // 毫秒内拿到重复 id 触发 23505 pkey 冲突）。
    let mut pids = Vec::new();
    {
        let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
            1_577_836_800_000,
            1,
        );
        use hsh_erp_rust::infra::clock::now_naive;
        for i in 0..5 {
            let now = now_naive();
            let today = now.date();
            let id = snowflake.next_id();
            // 字段对齐 `insert_part_with_status`：name 复用为 applicant_name。
            // drawing_no 硬编码 'D-001'；$7=assembly_id=NULL；$8=status='PENDING'。
            sqlx::query!(
                "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
                 applicant_name, request_date, planned_delivery_date, \
                 quantity, has_been_repaired, version, created_at, created_by, updated_at, updated_by, \
                 assembly_id) \
                 VALUES ($1, $2, $3, 'D-001', $4, $8, $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL, $7)",
                id,
                format!("P{i:03}"),
                format!("P{i}"),
                l2,
                now,
                today,
                Option::<i64>::None, // $7 = assembly_id
                "PENDING",            // $8 = status
            )
            .execute(&pool)
            .await
            .expect("insert part");
            pids.push(id);
        }
    }

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts?customer_id={l2}&limit=2&offset=2"),
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
    // 默认 sort_by=CREATED_AT desc（所有 fixture created_at 几乎相同）
    // → 兜底按 id DESC。offset=2 取第 3、4 件，即 pids 中按 id DESC 排序的第 3、4 位。
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts/{pid}"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "detail: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["id"], pid.to_string());
    assert_eq!(env["data"]["status"], "PENDING");
    assert_eq!(env["data"]["customer_name"], "二厂");
    assert_eq!(env["data"]["l1_customer_name"], "F");
}

/// GET /parts/{nonexistent_id} —— 20101 BIZ_PART_NOT_FOUND（HTTP 404）。
#[tokio::test]
async fn get_part_detail_404_not_found() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;

    // 软删 (SQL 直删 — 绕开 RBAC)
    sqlx::query!(
        "UPDATE t_part SET deleted_at = now(), version = version + 1 WHERE id = $1",
        pid
    )
    .execute(&pool)
    .await
    .unwrap();

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/parts/{pid}"),
            None::<Value>,
            Some(&token),
        ),
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
                "customer_id": l2.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create 201: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["name"], "test part");
    assert_eq!(env["data"]["status"], "PENDING");
    assert_eq!(env["data"]["customer_id"], l2.to_string());
    assert!(!env["data"]["id"].as_str().unwrap().is_empty());
}

/// POST /parts —— INSPECTOR 角色 → 40300 FORBIDDEN。
#[tokio::test]
async fn create_part_403_inspector() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let (app, token, _pool) = login_inspector(pool, "insp1").await;
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
                "customer_id": l2.to_string(),
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
                "customer_id": l2.to_string(),
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch",
            Some(json!({
                "customer_id": l2.to_string(),
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
///
/// 触发策略：PartBatchCreateItem 没有 `serial_no` 字段（系统生成），
/// 而 t_part 上唯一的 partial unique 是 `uk_t_part_serial_no`（仅在 serial_no
/// 非 NULL 时生效）；要触发 23505 必须先把一个 serial_no 占号，再用相同
/// serial_no 创建——但 batch_create_items 不接受 serial_no，路径不可达。
///
/// 替代触发：`applicant_name` 是 `varchar(50)`，超长字符串触发 22001
/// (string_data_right_truncation) → 50001 DATABASE，被 `map_create_error`
/// 兜底后送进 `failed`。
#[tokio::test]
async fn batch_create_parts_partial_failure() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let long_name = "甲".repeat(60); // >50 触发 22001
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch",
            Some(json!({
                "customer_id": l2.to_string(),
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
    // 50001 DATABASE — `applicant_name` varchar(50) 超长 → DB 22001
    assert_eq!(failed[0]["code"], 50001, "DATABASE 兜底码: {env}");
    assert_eq!(failed[0]["item_index"], 1);
}

/// POST /parts/batch —— `[bad, ok, ok]` 三件：第 1 件失败，后续 2 件仍能成功。
///
/// 这是 peer-review 找出的 savepoint bug 的回归测试。原代码把整批
/// INSERT 包在同一事务里但没用 SAVEPOINT；一旦第 1 件触发 22001 整个事务
/// 状态变 aborted，后续 INSERT 全都返回 `25P02 current transaction is
/// aborted`，全部被映射成 50001 DATABASE。修复后：每件 INSERT 前后
/// `SAVEPOINT batch_item_{idx}` / `RELEASE` / `ROLLBACK TO`，让外层事务
/// 保持可写。
#[tokio::test]
async fn batch_create_parts_savepoint_recovers_after_failure() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let long_name = "甲".repeat(60); // >50 触发 22001
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/parts/batch",
            Some(json!({
                "customer_id": l2.to_string(),
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
    // failed[0] = item_index=0（bad-item，applicant_name 超长）
    assert_eq!(failed[0]["item_index"], 0, "失败项应位于 idx=0: {env}");
    assert_eq!(failed[0]["code"], 50001, "DATABASE 兜底码（22001）: {env}");
}

// ===========================================================================
//  Tests — Part 3: update / soft-delete
// ===========================================================================

/// POST /parts/{id}/update —— 改 name + is_urgent → 200 / name 更新 / version 自增。
#[tokio::test]
async fn update_part_200() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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

/// POST /parts/{id}/update —— 已软删件 → 40901 VERSION_CONFLICT（update 守卫 deleted_at IS NULL）。
#[tokio::test]
async fn update_part_404_soft_deleted() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;

    // 软删 — 直接 SQL 写 deleted_at（绕开 RBAC；测试 update 守卫）
    sqlx::query!(
        "UPDATE t_part SET deleted_at = now(), version = version + 1 WHERE id = $1",
        pid
    )
    .execute(&pool)
    .await
    .unwrap();

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;

    let (app, token, pool) = login_manager(pool, "mgr").await;
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

    // 校验 DB 中 deleted_at 已设置
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;

    let (app, token, _pool) = login_clerk(pool, "clerk1").await;
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

/// POST /parts/{id}/soft-delete —— 重复软删同一件（已软删 + 现版本）→ 20101 BIZ_PART_NOT_FOUND (404)。
///
/// service 内：soft_delete SQL 返回 0 行 → get_by_id(include_deleted=true) → Some(p)
/// → 分支 `p.deleted_at.is_some()` → 20101, "已软删"。HTTP=404。
#[tokio::test]
async fn soft_delete_part_404_soft_deleted() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;

    // 第一次软删（直接 SQL —— 走 API 也能，这里模拟 "DB 已经软删" 的状态）
    sqlx::query!(
        "UPDATE t_part SET deleted_at = now(), version = version + 1 WHERE id = $1",
        pid
    )
    .execute(&pool)
    .await
    .unwrap();

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    // 第二次用现版本（version=1）+ soft-delete 端点 — 返回 0 行 → 20101
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

/// POST /parts/{id}/soft-delete —— PENDING + version 不匹配 → 40901 VERSION_CONFLICT (409)。
///
/// service 内：soft_delete SQL 返回 0 行 → get_by_id(include_deleted=true) → Some(p)
/// → 分支 `p.version != expected_version` → 40901。
#[tokio::test]
async fn soft_delete_part_409_version_conflict() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // 初始 version=0；客户端传 99 触发不匹配。
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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

/// POST /parts/{id}/soft-delete —— DELIVERED 终态 → 20119 BIZ_PART_NOT_DELETABLE (409)。
///
/// service 内：soft_delete SQL 返回 0 行（DELIVERED 命中守卫）→
/// get_by_id(include_deleted=true) → Some(p) → 分支 `status IN ('DELIVERED','COMPLETED')`
/// → 20119。
#[tokio::test]
async fn soft_delete_part_409_terminal_status() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "DELIVERED",
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("T-LOC-1"),
        None,
        "PENDING",
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "mgr").await;
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

/// 读取批次当前乐观锁版本（lifecycle batch 级 OCC payload 用，PR-B3 起）。
async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
}

/// POST /parts/{id}/deliver —— READY_TO_SHIP → DELIVERED (200 + status)。
///
/// 2026-09-11 PR-B3：lifecycle 收 `batch_id` + `version`（锚 batch.version）。
#[tokio::test]
async fn deliver_ready_to_ship_200() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "READY_TO_SHIP",
    )
    .await;
    // PR-B3：必须配一条 READY_TO_SHIP 批次；service 端按 batch.version 守卫。
    let bid = insert_batch(&pool, pid, 1, 1, "READY_TO_SHIP").await;
    let bver = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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

/// POST /parts/{id}/deliver —— batch 当前 INSPECTION → 20117 BIZ_PART_NOT_READY_TO_SHIP (HTTP 400)。
///
/// 2026-09-11 PR-B3：状态机守卫读 batch 状态（不是 part 派生列）。测试构造：
/// part=INSPECTION + batch=INSPECTION，service 应报 20117 而不是 20109（因为
/// batch 存在但状态不匹配）。
#[tokio::test]
async fn deliver_wrong_state_400() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "INSPECTION",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
    let bver = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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

/// POST /parts/{id}/cancel —— COMPLETED 状态不能 cancel → 20103 BIZ_INVALID_TRANSITION。
#[tokio::test]
async fn cancel_wrong_state_400() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // COMPLETED 不在 cancel 白名单内 (PENDING/PROGRAMMING/INSPECTION/READY_TO_SHIP/DELIVERED)
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        None,
        None,
        "COMPLETED",
    )
    .await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
///
/// 测试技巧：直接 INSERT status='DELIVERED'（最简洁路径，绕开 deliver 端点）。
/// 2026-09-11 PR-B3：lifecycle 收 `batch_id` + `version`（锚 batch.version）。
#[tokio::test]
async fn complete_delivered_200() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "DELIVERED").await;
    let bid = insert_batch(&pool, pid, 1, 1, "DELIVERED").await;
    let bver = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    // clear_part_serial_no_when_completed 清空 serial_no
    assert!(
        env["data"]["serial_no"].is_null(),
        "COMPLETED 应清空 serial_no: {env}"
    );
}

/// POST /parts/{id}/complete —— batch 当前 INSPECTION → 20116 BIZ_PART_NOT_DELIVERED。
///
/// 2026-09-11 PR-B3：状态机守卫读 batch 状态。测试构造 part=INSPECTION +
/// batch=INSPECTION，service 应报 20116 而不是 20109。
#[tokio::test]
async fn complete_wrong_state_400() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "INSPECTION").await;
    let bid = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
    let bver = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
///
/// 允许角色：Manager / Clerk / Inspector（任一即可）。
/// 2026-09-11 PR-B3：lifecycle 收 `batch_id` + `version`。
///
/// 2026-09-11 PR-B3：start_repair 把 `has_been_repaired=true` 写 batch（同 PR-B2 已
/// 实现）+ 单独物化 part（`mark_part_repairing_flag_only`，不被 rollup 覆盖）。
/// `PartOut` 当前不投影该字段（响应体最小化），故断言改为 DB 直查。
#[tokio::test]
async fn start_repair_in_process_200() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "IN_PROCESS",
    )
    .await;
    let bid = insert_batch(&pool, pid, 1, 1, "IN_PROCESS").await;
    let bver = batch_version(&pool, bid).await;

    let (app, token, pool) = login_manager(pool, "mgr").await;
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
    // start_repair 写 batch.has_been_repaired + 单独物化 part.has_been_repaired
    // （`mark_part_repairing_flag_only`，不在 rollup 范围内 —— 见
    // `src/modules/part/service/lifecycle.rs` start_repair 第 5 步注释）
    let part_hbr: bool = sqlx::query_scalar("SELECT has_been_repaired FROM t_part WHERE id = $1")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .expect("read part.has_been_repaired");
    assert!(part_hbr, "start-repair 应置 part.has_been_repaired=true");
}

/// POST /parts/{id}/start-repair —— batch PENDING → 20118 BIZ_PART_REPAIR_NOT_TRIGGERED。
///
/// 2026-09-11 PR-B3：状态机守卫读 batch 状态。part=PENDING + batch=PENDING，
/// service 应报 20118 而不是 20109。
#[tokio::test]
async fn start_repair_wrong_state_400() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 1, "PENDING").await;
    let bver = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    assert_eq!(s, StatusCode::BAD_REQUEST, "start-repair wrong state: {env}");
    assert_eq!(env["code"], 20118, "BIZ_PART_REPAIR_NOT_TRIGGERED: {env}");
}

/// POST /parts/{id}/deliver —— CANCELLED 状态 → 20115 BIZ_PART_ALREADY_CANCELLED (409)。
///
/// service 内新增 status guard：`from == CANCELLED` 一律 20115，
/// 走在 batch 定位之前。2026-09-11 PR-B3：必须构造有效 `batch_id` + `version`
/// 入参才能越过 axum Json 反序列化命中 service guard。
#[tokio::test]
async fn deliver_cancelled_409() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "CANCELLED",
    )
    .await;
    // 配任意活跃 batch（service guard 在 batch 定位前短路，不查 batch 状态）。
    let bid = insert_batch(&pool, pid, 1, 1, "READY_TO_SHIP").await;
    let bver = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
async fn batch_status_and_version(pool: &PgPool, part_id: i64, status: &str) -> Option<(String, i32)> {
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
///
/// Fix Batch 2 Finding A 回归测试（PR-B3 适配）：t_part_batch 不再 stale；
/// 通过 `batch_id` + `version` 指定操作批次。
#[tokio::test]
async fn deliver_also_updates_batch() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "READY_TO_SHIP",
    )
    .await;
    // 同 part 配一条 READY_TO_SHIP 批次
    let bid = insert_batch(&pool, pid, 1, 1, "READY_TO_SHIP").await;
    let bver = batch_version(&pool, bid).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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

    // 批次也应被翻到 DELIVERED
    let (batch_status, _) =
        batch_status_and_version(&_pool, pid, "DELIVERED")
            .await
            .expect("批次应存在并已被翻为 DELIVERED");
    assert_eq!(batch_status, "DELIVERED", "batch.status 应同步翻为 DELIVERED");
}

/// POST /parts/{id}/cancel —— PENDING → CANCELLED 应同时翻转最近一条
/// PENDING 批次到 CANCELLED（同事务）。
///
/// Fix Batch 2 Finding A 回归测试：cancel 流同样需 batch 同步。
#[tokio::test]
async fn cancel_also_updates_batch() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;
    let _bid = insert_batch(&pool, pid, 1, 1, "PENDING").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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

    let (batch_status, _) =
        batch_status_and_version(&_pool, pid, "CANCELLED")
            .await
            .expect("批次应存在并已被翻为 CANCELLED");
    assert_eq!(batch_status, "CANCELLED", "batch.status 应同步翻为 CANCELLED");
}

/// POST /parts/{id}/deliver —— batch_id 不存在 → 20109 BIZ_PART_BATCH_NOT_FOUND。
///
/// 2026-09-11 PR-B3：lifecycle 三端点必须传 `batch_id`。`batch_id` 不存在
/// 或不属于该 part → 20109。
#[tokio::test]
async fn deliver_without_source_batch_409() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "READY_TO_SHIP",
    )
    .await;
    // 故意不插 batch，传一个伪造 batch_id
    let fake_bid: i64 = 9_999_999_999;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
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
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "deliver w/o batch: {env}"
    );
    assert_eq!(env["code"], 20109, "BIZ_PART_BATCH_NOT_FOUND: {env}");
}

/// POST /parts/{id}/cancel —— part 已挂送货单 → 21420 BIZ_DELIVERY_NOTE_LOCKED_PART (HTTP 409)。
///
/// Fix Batch 2 Finding D 回归测试：cancel 流增加 delivery_note_id 守卫。
/// service 内通过 `PartRepo::get_part_detail` 取完整 TPart（含 delivery_note_id），
/// 锁定时返回 21420。
#[tokio::test]
async fn cancel_delivery_note_locked_409() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(
        &pool,
        "P0",
        l2,
        Some("P000"),
        None,
        "PENDING",
    )
    .await;

    // 直接 SQL 模拟「part 已挂送货单」：随便写一个 delivery_note_id。
    // 21420 不要求送货单实际存在 —— service 层只看 part.delivery_note_id 是否非 NULL。
    sqlx::query!(
        "UPDATE t_part SET delivery_note_id = 88888888 WHERE id = $1",
        pid
    )
    .execute(&pool)
    .await
    .unwrap();

    let (app, token, pool2) = login_manager(pool, "mgr").await;
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
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "delivery_note 锁 cancel: {env}"
    );
    assert_eq!(
        env["code"], 21420,
        "BIZ_DELIVERY_NOTE_LOCKED_PART: {env}"
    );

    // 确认 part.status 没被翻（事务回滚）
    let row: (String,) = sqlx::query_as("SELECT status FROM t_part WHERE id = $1")
        .bind(pid)
        .fetch_one(&pool2)
        .await
        .unwrap();
    assert_eq!(row.0, "PENDING", "锁定 part 不应被 cancel");
}

// ===========================================================================
//  Tests — Part 6: upload-drawing (multipart, #[ignore] for this round)
// ===========================================================================

/// POST /parts/{id}/upload-drawing —— multipart PDF → 21106 ish;
/// NoopCos 模拟下应成功落 t_part_file (kInd=DRAWING)。
///
/// 本轮 client 不构造 multipart body，留 `#[ignore]` 等后续用 reqwest / hyper
/// client 拼接 multipart/form-data 单独覆盖。
#[tokio::test]
#[ignore = "multipart body 构造需 reqwest client，本轮占位"]
async fn upload_drawing_integration() {
    // 实现思路（占位）：
    //   let part_id = ...; // 创建 PENDING 工单
    //   let bytes = include_bytes!("../../fixtures/test-drawing.pdf");
    //   let form = reqwest::multipart::Part::bytes(bytes.to_vec())
    //       .file_name("test.pdf")
    //       .mime_str("application/pdf").unwrap();
    //   let multipart = reqwest::multipart::Form::new()
    //       .part("file", form);
    //   let resp = reqwest::Client::new()
    //       .post(&format!("http://.../parts/{part_id}/upload-drawing"))
    //       .bearer_auth(token)
    //       .multipart(multipart)
    //       .send().await.unwrap();
    //   assert_eq!(resp.status(), 200);
}

// ===========================================================================
//  Tests — Part 6.5: 上传 service 层（2026-09-11 新增）
//
//  走 service 层直接调用，绕过 multipart HTTP 复杂度，验证：
//  - NoopCos 静默成功 → t_part_file 落 DRAWING / 3D_MODEL 行
//  - 扩展名 / content_type 白名单拦截
//  - CAS key 格式符合 Python 同款（`{prefix}/{owner_kind}/{id}/{KIND}/{sha16}_{safe}`）
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
#[ignore] // 跑 service 层需起 postgres-test + redis-test；CI 集成测试再开启
async fn upload_drawing_service_integration() {
    let _ = TEST_LOCK.lock().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;

    let state = test_state_with_disabled_session(pool.clone());
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 99); // 99 区分测试进程，避免 ID 冲突

    // 建 L1+L2 客户 → PENDING part
    let l1 = insert_l1(&pool, "U", "U").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part_with_status(&pool, "P-DRAW", l2, None, None, "PENDING").await;

    let bytes = fake_pdf_bytes();
    let mut conn = state.pool.begin().await.unwrap();
    let pf = PartService::upload_drawing(
        &mut conn,
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

    // 验证 t_part_file 行
    assert_eq!(pf.kind, "DRAWING");
    assert_eq!(pf.file_type, "PDF");
    assert_eq!(pf.part_id, part_id);
    assert_eq!(pf.original_filename, "drawing.pdf");
    assert_eq!(pf.file_size, bytes.len() as i64);
    assert_eq!(pf.content_type, "application/pdf");
    assert_eq!(pf.upload_status, "READY");
    assert!(pf.content_sha256.is_some());
    // CAS key 格式：`uploads/part/{id}/DRAWING/{sha16}_{safe}.pdf`
    assert!(
        pf.object_key.starts_with(&format!("uploads/part/{part_id}/DRAWING/")),
        "CAS key 格式不符: {}",
        pf.object_key
    );
    assert!(pf.object_key.ends_with("_drawing.pdf"));

    // 反查 DB
    let row = PartFileRepo::get_by_part_kind(&pool, part_id, "DRAWING")
        .await
        .unwrap()
        .expect("DB 行应存在");
    assert_eq!(row.id, pf.id);
}

#[tokio::test]
#[ignore]
async fn upload_3d_model_service_integration() {
    let _ = TEST_LOCK.lock().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;

    let state = test_state_with_disabled_session(pool.clone());
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 99);

    let l1 = insert_l1(&pool, "U", "U").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part_with_status(&pool, "P-3D", l2, None, None, "PENDING").await;

    let bytes = fake_step_bytes();
    let mut conn = state.pool.begin().await.unwrap();
    let pf = PartService::upload_3d_model(
        &mut conn,
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
    assert!(pf.object_key.starts_with(&format!("uploads/part/{part_id}/3D_MODEL/")));
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
    // 2026-09-11 新增：DRAWING kind 不接受 .step 扩展名，应 BIZ_PART_FILE_BAD_TYPE
    let _ = TEST_LOCK.lock().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;

    let state = test_state_with_disabled_session(pool.clone());
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 99);
    let l1 = insert_l1(&pool, "U", "U").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part_with_status(&pool, "P-BAD", l2, None, None, "PENDING").await;

    let mut conn = state.pool.begin().await.unwrap();
    let err = PartService::upload_drawing(
        &mut conn,
        &snowflake,
        &state,
        part_id,
        b"junk".to_vec().as_slice(),
        "evil.step", // STEP 扩展名给 DRAWING 端点
        "application/step",
        &manager_current(1),
    )
    .await
    .expect_err("应被扩展名白名单拦截");
    conn.rollback().await.unwrap();

    // 错误码断言：policy::allowed_exts("DRAWING") == ["pdf"]，"step" 不在 → BAD_TYPE
    assert_eq!(err.code(), code::BIZ_PART_FILE_BAD_TYPE, "实际错误: {err:?}");
}

#[tokio::test]
#[ignore]
async fn upload_content_type_mismatch_rejected() {
    // 2026-09-11 新增：扩展名是 .pdf 但 content_type 不对，应 BIZ_PART_FILE_BAD_TYPE
    let _ = TEST_LOCK.lock().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;

    let state = test_state_with_disabled_session(pool.clone());
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 99);
    let l1 = insert_l1(&pool, "U", "U").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part_with_status(&pool, "P-CT", l2, None, None, "PENDING").await;

    let mut conn = state.pool.begin().await.unwrap();
    let err = PartService::upload_drawing(
        &mut conn,
        &snowflake,
        &state,
        part_id,
        b"junk".to_vec().as_slice(),
        "drawing.pdf",
        "image/png", // 扩展名 PDF 但 content_type 不匹配
        &manager_current(1),
    )
    .await
    .expect_err("应被 content_type 校验拦截");
    conn.rollback().await.unwrap();

    assert_eq!(err.code(), code::BIZ_PART_FILE_BAD_TYPE);
}

// ===========================================================================
//  2026-09-16 M2-B review 第 2 轮 B1 修：cleanup_tmp_keys 全量收集回归测试
//
// 验证 service `batch_create_parts_with_bindings` 在 per-item DB 失败时仍然把
// 所有 head/copy 成功的 tmp_key 收集到 `out.cleanup_tmp_keys`（B1 不变量）：
// - 失败 item 的 tmp 对象：DB 没 INSERT part_file 行，但 CAS 对象已在 COS →
//   必须删 tmp 防孤儿（DB 端可下次 batch 重传时复用 tmp_key，tmp 已被 head
//   校验过 sha/size）。
// - 成功 item 的 tmp 对象：DB 已 INSERT part_file 行指向 cas_key → 删 tmp
//   是无害（CAS 完整性由 part_file.object_key 引用保证）。
//
// 触发策略：2 件 item 都带 drawing_file，item[0] applicant_name > 50 触发 DB 22001。
// MockCos 预注册 head/copy 成功。期望 out.created=1 / out.failed=1 / cleanup_tmp_keys=2。
// ===========================================================================

#[tokio::test]
async fn batch_create_with_bindings_partial_failure_cleans_all_tmp() {
    use common::MockCos;
    use hsh_erp_rust::modules::part::dto_crud::{
        FileBindingIn, PartBatchCreateItem, PartBatchCreateRequest,
    };

    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

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
    let long_name = "甲".repeat(60); // >50 触发 22001 → failed
    let req = PartBatchCreateRequest {
        customer_id: l2,
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
    let out = PartService::batch_create_parts_with_bindings(
        &mut tx,
        &snowflake,
        cos.clone(),
        "uploads",
        "tmp/",
        &req,
        &current,
    )
    .await
    .expect("batch_create_parts_with_bindings 应整体返回 Ok（含 failed）");
    tx.commit().await.unwrap();

    // 1) item 0 DB 失败 → failed.len()=1；item 1 成功 → created.len()=1
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

    // 2) B1 不变量：cleanup_tmp_keys 必须包含**所有** head/copy 成功的 tmp_key，
    //    与 per-item DB 结果无关。失败 item 的 tmp_key 也必须在里面。
    let keys = &out.cleanup_tmp_keys;
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

    // 3) head + copy 都被调用各 1 次（每个 binding 一份）
    assert_eq!(cos.head_call_count(tmp_key_0), 1, "item 0 head 应被调 1 次");
    assert_eq!(cos.head_call_count(tmp_key_1), 1, "item 1 head 应被调 1 次");
    let copy_calls = cos.copy_calls.lock().unwrap().clone();
    assert_eq!(
        copy_calls.len(),
        2,
        "copy_object 应被调 2 次（每个 binding 一份）: got {copy_calls:?}"
    );

    // 4) service 层不直接 spawn delete（这是 handler 的职责）；delete_calls 此时应为空。
    //    handler 端的 spawn-delete 行为由后续端到端测试覆盖（受 part_crud 当前 fixture
    //    限制——state.cos 是 NoopCos，硬替换 AppState.cos 工作量过大；service 层
    //    cleanup_tmp_keys 已足够验证 B1 不变量）。
    assert_eq!(
        cos.delete_call_count(tmp_key_0),
        0,
        "service 层不应直接 spawn delete（属 handler 职责）"
    );
}

#[tokio::test]
#[ignore]
async fn policy_unit_smoke_integration() {
    // 2026-09-11 新增：service 层直接验证 policy::allowed_exts / file_type_for_ext
    // 已被 cargo test --lib 覆盖；这里仅冒烟，防止 policy 表未来被改坏时无声回归
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
