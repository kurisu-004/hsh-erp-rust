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

use common::{add_role, insert_user_with_password, test_app, test_state};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

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

    // 3 个 PENDING + 1 个 INSPECTION（都在 L2 下）
    for i in 0..3 {
        insert_part_with_status(
            &pool,
            &format!("P{i}"),
            l2,
            Some(&format!("P{i:03}")),
            None,
            "PENDING",
        )
        .await;
    }
    insert_part_with_status(
        &pool,
        "PINSP",
        l2,
        Some("PINS"),
        None,
        "INSPECTION",
    )
    .await;

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

/// POST /parts/{id}/deliver —— READY_TO_SHIP → DELIVERED (200 + status)。
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

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({ "note": "发货" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "deliver 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "DELIVERED");
}

/// POST /parts/{id}/deliver —— INSPECTION → 20117 BIZ_PART_NOT_READY_TO_SHIP (HTTP 400)。
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

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({})),
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
#[tokio::test]
async fn complete_delivered_200() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "DELIVERED").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/complete"),
            Some(json!({ "note": "归档" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "COMPLETED");
    // mark_part_completed 会清空 serial_no
    assert!(
        env["data"]["serial_no"].is_null(),
        "COMPLETED 应清空 serial_no: {env}"
    );
}

/// POST /parts/{id}/complete —— INSPECTION 状态 → 20116 BIZ_PART_NOT_DELIVERED。
#[tokio::test]
async fn complete_wrong_state_400() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "INSPECTION").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/complete"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "complete wrong state: {env}");
    assert_eq!(env["code"], 20116, "BIZ_PART_NOT_DELIVERED: {env}");
}

/// POST /parts/{id}/start-repair —— IN_PROCESS → REPAIRING (200 + status)。
///
/// 允许角色：Manager / Clerk / Inspector（任一即可）。
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

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/start-repair"),
            Some(json!({ "reason": "尺寸偏大" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start-repair 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["status"], "REPAIRING");
}

/// POST /parts/{id}/start-repair —— PENDING 状态 → 20118 BIZ_PART_REPAIR_NOT_TRIGGERED。
#[tokio::test]
async fn start_repair_wrong_state_400() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, Some("P000"), None, "PENDING").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/start-repair"),
            Some(json!({ "reason": "no-op" })),
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
/// 走在 wrong-state 检查（20117 BIZ_PART_NOT_READY_TO_SHIP）之前。
/// 同样的 guard 也加在 `complete` / `start_repair`，这里只验 deliver。
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

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({})),
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
/// 最近一条 READY_TO_SHIP 批次到 DELIVERED（同事务）。
///
/// Fix Batch 2 Finding A 回归测试：t_part_batch 不再 stale。
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
    let _bid = insert_batch(&pool, pid, 1, 1, "READY_TO_SHIP").await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({ "note": "发货" })),
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

/// POST /parts/{id}/deliver —— 工单无对应 source-status 批次时仍应成功
/// （t_part 单独翻转合法）。
///
/// Fix Batch 2 Finding A 边界测试：`find_most_recent_batch_for_part` 返回
/// `None` 时跳过 `mark_batch_*`，不报错。
#[tokio::test]
async fn deliver_without_source_batch_ok() {
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
    // 故意不插 batch

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/deliver"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "deliver w/o batch: {env}");
    assert_eq!(env["data"]["status"], "DELIVERED");
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
