//! applicant 域 6 端点集成测试（Task 5 / plan §11）
//!
//! 覆盖：
//!   1. list_applicants_empty                          — 空 DB GET /applicants → items=[]
//!   2. create_get_update_soft_delete_applicant_happy_path — 全生命周期 + version 递增 + 软删后 404
//!   3. create_with_l2_customer_returns_21003          — L1 校验
//!   4. duplicate_name_under_same_customer_returns_21002 — 同客户下重名
//!   5. update_with_stale_version_returns_409          — 乐观锁（并发事务让 UPDATE 撞 0 行）
//!   6. soft_delete_in_use_returns_21004               — 被 t_part.applicant_name 引用 → 拒软删
//!
//! ## 并行 / 认证
//! 共享 `postgres_rust_test` 库；进程级 `tokio::sync::Mutex` 串行化。
//! 每个用例 MANAGER token；与其它 applicant 域用例共享同一 token 来源（用户独立）。
//!
//! ## URL 约定
//! `test_app` 返回 `v2_router()` 直挂，无 `/api/v2` 前缀 —— 故 URL 写 `/applicants` 而非
//! `/api/v2/applicants`（与 main.rs 的 `/api/v2` nest 区分）。该写法与 worker_pool_api.rs /
//! part_api.rs / delivery_*_api.rs 等保持一致。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, ensure_database_exists, insert_user_with_password,
    test_app, test_pool, test_state,
};

// ===========================================================================
//  全局串行化 + HTTP helpers（沿用 worker_pool_api.rs / part_api.rs 风格）
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
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

// ----- 角色登录 helper -----

/// 建一个 MANAGER 用户并登录，返回 (Router, access_token, PgPool)。
///
/// `app2` 是用于后续请求的 Router；与登录请求用的 Router 同 state 但独立实例。
/// 注意：登录必须消耗一个 Router（oneshot 拿走），所以单独构造 login_app。
async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;
    let state = test_state(pool.clone()).await;
    let login_app = test_app(state.clone());
    let (_, env) = send(
        login_app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": username, "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app = test_app(state);
    (app, token, pool)
}

// ----- Fixture helpers（applicant 域专用）-----

/// 插一个 L1 客户（parent_id IS NULL）；serial_prefix 取 `name` 首字母大写。
async fn insert_l1(pool: &PgPool, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    // serial_prefix 是 varchar(1) + regex ^[A-Z]$ —— 取 name 首字母大写；fallback 'X'
    let one_char: String = name
        .chars()
        .next()
        .unwrap_or('X')
        .to_ascii_uppercase()
        .to_string();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id,
        name,
        one_char,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1 customer");
    id
}

/// 插一个 L2 客户（parent_id = l1_id）；serial_prefix 留 NULL（叶子节点无前缀）。
async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id,
        name,
        l1_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L2 customer");
    id
}

/// 插一条引用指定 `(applicant_name, customer_id)` 的 t_part（未软删）。
/// service 层 `count_parts_using_applicant_name` 据此判定 in-use。
/// 返回 part.id。
async fn insert_part_referencing_applicant(
    pool: &PgPool,
    customer_id: i64,
    applicant_name: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query!(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'D-PART', $3, $4, $5, $5, 'PENDING', 0, $6, NULL, $6, NULL)",
        id,
        format!("part-for-{applicant_name}"),
        applicant_name,
        customer_id,
        today,
        now,
    )
    .execute(pool)
    .await
    .expect("insert referencing t_part");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. 空 DB：GET /applicants → 200 / items=[] / total=0 / limit=100（DEFAULT_LIMIT）。
#[tokio::test]
async fn list_applicants_empty() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "admin_empty").await;

    let (s, env) = send(
        app,
        json_request("GET", "/applicants", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list empty: {env}");
    assert_eq!(env["code"], 0);
    assert!(env["data"]["items"].is_array());
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 0);
    assert_eq!(env["data"]["total"], 0);
    assert_eq!(env["data"]["limit"], 100, "DEFAULT_LIMIT 应=100: {env}");
    assert_eq!(env["data"]["offset"], 0);
}

/// 2. 全生命周期 happy path：create → get → update → soft-delete → get 404。
/// 校验：
/// - create 后 version=0 / customer_name 已 join
/// - update 后 name 改变 + version=1（OCC 自增）
/// - soft-delete 后 GET 返 404 / code=21001 BIZ_APPLICANT_NOT_FOUND
#[tokio::test]
async fn create_get_update_soft_delete_applicant_happy_path() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "admin_crud").await;
    let l1 = insert_l1(&_pool, "TestCo-Crud").await;

    // create
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/applicants",
            Some(json!({"name": "张三", "customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    assert_eq!(env["code"], 0);
    let id = env["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(env["data"]["name"], "张三");
    assert_eq!(env["data"]["customer_id"], l1.to_string());
    assert_eq!(env["data"]["customer_name"], "TestCo-Crud");
    assert_eq!(env["data"]["version"], 0);

    // get
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/applicants/{id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "get: {env}");
    assert_eq!(env["data"]["id"], id);
    assert_eq!(env["data"]["name"], "张三");
    assert_eq!(env["data"]["customer_name"], "TestCo-Crud");

    // update
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/applicants/{id}/update"),
            Some(json!({"name": "李四"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "update: {env}");
    assert_eq!(env["data"]["name"], "李四");
    assert_eq!(env["data"]["version"], 1, "update 应 OCC +1: {env}");

    // soft-delete
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/applicants/{id}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "soft-delete: {env}");
    assert_eq!(env["code"], 0);

    // 软删后 GET → 404 / code=21001
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/applicants/{id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "软删后 GET 应 404: {env}");
    assert_eq!(env["code"], 21001, "BIZ_APPLICANT_NOT_FOUND: {env}");
}

/// 3. customer_id 指向 L2（非一级） → POST → 400 / 21003 BIZ_APPLICANT_BAD_CUSTOMER。
#[tokio::test]
async fn create_with_l2_customer_returns_21003() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "admin_l2").await;
    let l1 = insert_l1(&_pool, "L2CoRoot").await;
    let l2 = insert_l2(&_pool, "L2Child", l1).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/applicants",
            Some(json!({"name": "王五", "customer_id": l2.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "L2 应 400: {env}");
    assert_eq!(env["code"], 21003, "BIZ_APPLICANT_BAD_CUSTOMER: {env}");
}

/// 4. 同一 L1 下姓名重复 → POST → 409 / 21002 BIZ_APPLICANT_DUPLICATE_NAME。
#[tokio::test]
async fn duplicate_name_under_same_customer_returns_21002() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "admin_dup").await;
    let l1 = insert_l1(&_pool, "DupCo").await;

    // 第一次创建同名成功
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/applicants",
            Some(json!({"name": "重复名", "customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "第一次 create: {env1}");

    // 第二次同名 → 409 / 21002
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/applicants",
            Some(json!({"name": "重复名", "customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "重名应 409: {env2}");
    assert_eq!(env2["code"], 21002, "BIZ_APPLICANT_DUPLICATE_NAME: {env2}");
}

/// 5. 乐观锁：并发 update 同一行，两个事务同时 SELECT V=0 → 两个都准备
///    UPDATE WHERE V=0 → 第二个 COMMIT 时 UPDATE 命中 0 行 → 40901 VERSION_CONFLICT。
///
/// 设计：tokio::join! 同时跑两个独立 Router（共享 Arc<AppState> + PgPool）的 update；
/// 由于 service 的"SELECT V THEN UPDATE WHERE V"两步在同一事务内，
/// READ COMMITTED 下两个并发事务会出现：一个先 UPDATE 提交 → 另一个 UPDATE 撞 0 行。
///
/// 断言：恰好 1 个 200 + 1 个 409。
#[tokio::test]
async fn update_with_stale_version_returns_409() {
    let (_guard, pool) = setup().await;
    let (app1, token1, _pool) = login_manager(pool, "admin_occ_a").await;
    let (app2, token2, pool) = login_manager(_pool, "admin_occ_b").await;
    let l1 = insert_l1(&pool, "OccCo").await;

    // 建一个 applicant V=0
    let (s, env) = send(
        app1.clone(),
        json_request(
            "POST",
            "/applicants",
            Some(json!({"name": "occ-target", "customer_id": l1.to_string()})),
            Some(&token1),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    let id = env["data"]["id"].as_str().unwrap().to_string();

    // 两个并发 update（不同名字）—— 一个会成功 V=0→1，另一个会撞 stale version → 40901
    let uri = format!("/applicants/{id}/update");
    let req_a = json_request(
        "POST",
        &uri,
        Some(json!({"name": "name-A"})),
        Some(&token1),
    );
    let req_b = json_request(
        "POST",
        &uri,
        Some(json!({"name": "name-B"})),
        Some(&token2),
    );
    let (r1, r2) = tokio::join!(send(app1, req_a), send(app2, req_b));
    let (s_a, e_a) = r1;
    let (s_b, e_b) = r2;

    // 期望：恰好一个 200，一个 409
    let pair = [(s_a, &e_a), (s_b, &e_b)];
    let ok_count = pair.iter().filter(|(s, _)| *s == StatusCode::OK).count();
    let conflict_count = pair.iter().filter(|(s, _)| *s == StatusCode::CONFLICT).count();
    assert_eq!(
        ok_count, 1,
        "exactly one update should succeed: A={s_a}/{e_a}; B={s_b}/{e_b}"
    );
    assert_eq!(
        conflict_count, 1,
        "exactly one update should hit OCC 409: A={s_a}/{e_a}; B={s_b}/{e_b}"
    );
    let conflict_env = pair
        .iter()
        .find(|(s, _)| *s == StatusCode::CONFLICT)
        .map(|(_, e)| e)
        .expect("find conflict response");
    assert_eq!(
        conflict_env["code"], 40901,
        "VERSION_CONFLICT: A={s_a}/{e_a}; B={s_b}/{e_b}"
    );
}

/// 6. in-use 校验：先建 applicant → 插 t_part 引用此 applicant_name + customer_id →
///    soft-delete → 409 / 21004 BIZ_APPLICANT_IN_USE。
#[tokio::test]
async fn soft_delete_in_use_returns_21004() {
    let (_guard, pool) = setup().await;
    let (app, token, _pool) = login_manager(pool, "admin_inuse").await;
    let l1 = insert_l1(&_pool, "InUseCo").await;

    // 建 applicant
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/applicants",
            Some(json!({"name": "被引用名", "customer_id": l1.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    let id = env["data"]["id"].as_str().unwrap().to_string();

    // 插 t_part 引用此 applicant_name + L1 customer_id
    let _ = insert_part_referencing_applicant(&_pool, l1, "被引用名").await;

    // soft-delete → 409 / 21004
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/applicants/{id}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "被引用应 409: {env}");
    assert_eq!(env["code"], 21004, "BIZ_APPLICANT_IN_USE: {env}");

    // 校验 applicant 仍存在（未被软删）
    let still_there: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!" FROM t_applicant WHERE id = $1 AND deleted_at IS NULL"#,
        id.parse::<i64>().unwrap(),
    )
    .fetch_one(&_pool)
    .await
    .expect("count applicant");
    assert_eq!(still_there, 1, "in-use 时 applicant 应未被软删");
}
