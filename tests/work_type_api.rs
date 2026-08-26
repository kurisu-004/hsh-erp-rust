//! work_type 域端到端集成测试
//!
//! ## 覆盖（Task 5 work_type CRUD + mapping）
//! 1. `create_work_type_then_set_processes_then_soft_delete_in_use` — 完整 happy path +
//!    soft-delete-in-use 拒：创建工种 → set 工序映射 → 试图软删 → 期望 20903 BIZ_WORK_TYPE_IN_USE。
//!
//! ## 并行
//! 所有用例共享 `postgres_rust_test` + `uk_t_work_type_code` 唯一约束，用
//! 进程级 `tokio::sync::Mutex` 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（写路径要求 M-only，按设计 §6.1 用 M 即可）。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, ensure_database_exists, insert_user_with_password,
    seed_process, test_app, test_pool, test_state,
};

// ===========================================================================
// 全局串行化 + helpers（与 customer_api.rs / process_api.rs / shelf_api.rs /
// worker_api.rs 同形）
// ===========================================================================
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let uri = req.uri().to_string();
    let method = req.method().to_string();
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body_str = String::from_utf8_lossy(&body).to_string();
    let envelope: Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!(
            "parse JSON: {e}; method={method} uri={uri} status={status}; raw = {body_str:?}"
        )
    });
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

// ===========================================================================
// Tests
// ===========================================================================

/// 完整 happy path：
/// 1. create work_type（含 description + sort_order + max_held_batches）
/// 2. 直接 raw SQL 插 `t_work_type_process` 行（绕开业务 set_work_type_processes，
///    仅为了制造引用，避免与 mapping 端点的语义耦合）
/// 3. soft-delete → 期望 409 + 20903 `BIZ_WORK_TYPE_IN_USE`
///    （service 层 UNION ALL 查 t_worker.work_type_id + t_work_type_process 引用）
#[tokio::test]
async fn create_work_type_then_set_processes_then_soft_delete_in_use() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "wt_admin").await;

    // 1) Create work type
    let (s_create, env_create) = send(
        app.clone(),
        json_request(
            "POST",
            "/work-types",
            Some(json!({
                "code": "WT-CNC",
                "name": "CNC Operator",
                "description": "5-axis CNC",
                "sort_order": 10,
                "max_held_batches": 3,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_create,
        StatusCode::CREATED,
        "create work type: {env_create}"
    );
    assert_eq!(env_create["code"], 0);
    let wt_id_str = env_create["data"]["id"].as_str().unwrap().to_string();
    let wt_id: i64 = wt_id_str.parse().unwrap();
    assert_eq!(env_create["data"]["code"], "WT-CNC");
    assert_eq!(env_create["data"]["name"], "CNC Operator");
    assert_eq!(env_create["data"]["description"], "5-axis CNC");
    assert_eq!(env_create["data"]["sort_order"], 10);
    assert_eq!(env_create["data"]["max_held_batches"], 3);

    // 2) 直接 raw SQL 插 `t_work_type_process` 行：seed 2 个工序 + 插映射
    let p1 = seed_process(&pool, "WT-CNC-P1", "CNC step 1").await;
    let p2 = seed_process(&pool, "WT-CNC-P2", "CNC step 2").await;
    insert_work_type_process_mapping(&pool, wt_id, p1).await;
    insert_work_type_process_mapping(&pool, wt_id, p2).await;

    // 3) Soft-delete → 期望 409 + 20903
    let (s_del, env_del) = send(
        app,
        json_request(
            "POST",
            &format!("/work-types/{wt_id_str}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_del,
        StatusCode::CONFLICT,
        "soft-delete in-use work type should return 409; got {env_del}"
    );
    assert_eq!(
        env_del["code"].as_i64().unwrap(),
        20903,
        "expected BIZ_WORK_TYPE_IN_USE; got envelope: {env_del}"
    );
}

/// 直插 `t_work_type_process` 行（无业务软删：`deleted_at` 留默认 NULL）。
///
/// 模拟 `set_work_type_processes` 写入后的状态，避免依赖同任务的 mapping 端点语义
/// 把「create work_type + soft-delete-in-use 拒」测试与「mapping 端点 happy path」耦在一起。
async fn insert_work_type_process_mapping(pool: &PgPool, wt_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
        1_577_836_800_000,
        1,
    );
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
        id,
        wt_id,
        p_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_work_type_process");
}