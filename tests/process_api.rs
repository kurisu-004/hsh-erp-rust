//! process 域端到端集成测试
//!
// 覆盖（Phase P2 process CRUD 段）：
// 1. create INHOUSE 强制 `requires_approval = false`（无视请求里的 true）
// 2. create 重复 code 拒 → 20802 BIZ_PROCESS_DUPLICATE_CODE
// 3. update 禁止改 `code` → 20104 BIZ_INVALID_VALUE
// 4. OUTSOURCE 类别保留 `requires_approval = true`（默认）
// 5. soft-delete 检查引用 → 20803 BIZ_PROCESS_IN_USE（挂 part.next_process_id）

// ## 并行
// 所有用例共享 `postgres_rust_test` + `uk_t_process_code` 唯一约束，用
// 进程级 `tokio::sync::Mutex` 串行化。
// ## 认证
// 用 MANAGER 用户跑通（POST /processes 写路径要求 M-only，按设计 §6.1 用 M 即可）。

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
// 全局串行化 + helpers（与 customer_api.rs / worker_pool_api.rs 同形）
// ===========================================================================
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let envelope: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("parse JSON: {e}; status={status}"));
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

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String) {
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
    (app2, token)
}

/// 直插一个 `t_part` 行（next_process_id = given），让 soft-delete 检查
/// 「被 part.next_process_id 引用」分支触发 20803 BIZ_PROCESS_IN_USE。
/// 绕开 part 域 CRUD（part CRUD 不是本任务范畴）。
async fn insert_part_with_next_process(pool: &PgPool, process_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
        1_577_836_800_000,
        1,
    );
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, next_process_id, version, created_at, updated_at) \
         VALUES ($1, 'TEST-NAME', 'TEST-DWG', 'TEST-APPLICANT', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $2, $3, 0, $4, $4)",
        id,
        // 用一个伪 customer_id (1L)；因为 soft-delete 检查只看 next_process_id，不校验 FK
        // 但 part 表 NOT NULL customer_id —— 用最小有效值 (1L)。truncate 会重置 sequence
        // 让它从 1 开始，所以这里直接选 1 即可。
        1_i64,
        process_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part referencing process");
    id
}

// ===========================================================================
// Tests
// ===========================================================================

/// INHOUSE 工序：请求体里的 `requires_approval=true` 必须被 service 层强制覆盖为 false
/// （与 Python `_assert_inhouse_no_approval` 对齐）。
#[tokio::test]
async fn create_process_inhouse_forces_requires_approval_false() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_inhouse").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-CUT",
                "name": "Cutting",
                "category": "INHOUSE",
                "requires_approval": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create INHOUSE: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["category"], "INHOUSE");
    assert_eq!(
        env["data"]["requires_approval"], false,
        "INHOUSE 必须强制 requires_approval=false; got: {env}"
    );
}

/// OUTSOURCE 工序：保留 `requires_approval` 默认 true。
#[tokio::test]
async fn create_process_outsource_keeps_requires_approval_default_true() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_outsrc").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-COAT",
                "name": "Coating",
                "category": "OUTSOURCE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create OUTSOURCE: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["category"], "OUTSOURCE");
    assert_eq!(
        env["data"]["requires_approval"], true,
        "OUTSOURCE 必须保留默认 requires_approval=true; got: {env}"
    );
}

/// 重复 code：撞 `uk_t_process_code` 部分唯一索引 → 20802 BIZ_PROCESS_DUPLICATE_CODE。
#[tokio::test]
async fn create_process_duplicate_code_returns_20802() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "proc_dup").await;

    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-DUP",
                "name": "First",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0, "first create should succeed; got: {env1}");

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-DUP",
                "name": "Second",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "duplicate code should return 409; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20802,
        "expected BIZ_PROCESS_DUPLICATE_CODE; got: {env2}"
    );
}

/// update 时改 `code` 必拒 → 20104 BIZ_INVALID_VALUE（code 是业务唯一键，不可变）。
#[tokio::test]
async fn update_process_code_change_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_code_lock").await;

    // 创建一个工序
    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-ORIG",
                "name": "Original",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    let pid = env1["data"]["id"].as_str().unwrap().to_string();

    // 试图改 code → 应拒
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/processes/{pid}/update"),
            Some(json!({"code": "P-NEW", "name": "Renamed"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::BAD_REQUEST,
        "code change should return 400; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20104,
        "expected BIZ_INVALID_VALUE; got: {env2}"
    );
}

/// 软删前查引用：`t_part.next_process_id` 仍有引用 → 20803 BIZ_PROCESS_IN_USE。
#[tokio::test]
async fn soft_delete_process_referenced_by_part_returns_20803() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "proc_in_use").await;

    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-IN-USE",
                "name": "Referenced",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    let pid_str = env1["data"]["id"].as_str().unwrap().to_string();
    let pid: i64 = pid_str.parse().unwrap();

    // 插一个 part.next_process_id = pid
    insert_part_with_next_process(&pool, pid).await;

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/processes/{pid_str}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "soft-delete in-use process should return 409; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20803,
        "expected BIZ_PROCESS_IN_USE; got: {env2}"
    );
}