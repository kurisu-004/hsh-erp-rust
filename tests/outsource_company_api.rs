//! outsource company 域集成测试（Phase 2 2026-09-13）
//!
//! 覆盖：
//! - list: 创建 3 个公司 + name_like 过滤 + is_active 过滤
//! - get: 详情含 process_ids 反查
//! - create: name 必填 + 重名 409 + 可选 process_ids 注入
//! - update: OCC 版本冲突 + 部分字段
//! - soft-delete: 仍映射工序时 409
//! - list-by-process: 按 process 反查 active 公司
//! - set-processes: 整体替换（delete-then-insert）

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
//  全局串行化 + helpers
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

/// 插一个 OUTSOURCE 类别 process（不走 common::seed_process）
async fn seed_outsource_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
        1_577_836_800_000,
        1,
    );
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert OUTSOURCE process");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_outsource_company_happy_path() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "oc_admin").await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "Acme 加工厂", "is_active": true})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["name"], "Acme 加工厂");
    assert_eq!(env["data"]["is_active"], true);
    assert_eq!(env["data"]["version"], 0);
    assert_eq!(env["data"]["processes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn create_outsource_company_duplicate_returns_21202() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "oc_dup").await;

    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "SameName"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "SameName"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "duplicate: {env2}");
    assert_eq!(env2["code"].as_i64().unwrap(), 21202);
}

#[tokio::test]
async fn create_outsource_company_with_process_ids_creates_mapping() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "oc_proc").await;
    let p1 = seed_outsource_process(&pool, "PROC-O-1", "外协工序1").await;
    let p2 = seed_outsource_process(&pool, "PROC-O-2", "外协工序2").await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({
                "name": "ProcMapping Co",
                "process_ids": [p1.to_string(), p2.to_string()]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create with procs: {env}");
    let cid = env["data"]["id"].as_str().unwrap().to_string();
    let procs = env["data"]["processes"].as_array().unwrap();
    assert_eq!(procs.len(), 2);
    assert_eq!(procs[0]["process_id"].as_str().unwrap(), p1.to_string());
    assert_eq!(procs[1]["process_id"].as_str().unwrap(), p2.to_string());

    // by-process 也能查到
    let (_, env_by) = send(
        app,
        json_request(
            "GET",
            &format!("/outsource-companies/by-process/{p1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let items = env_by["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), cid);
}

#[tokio::test]
async fn update_outsource_company_version_conflict_returns_40901() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "oc_vc").await;

    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "VC Co"})),
            Some(&token),
        ),
    )
    .await;
    let cid = env_c["data"]["id"].as_str().unwrap().to_string();

    // 故意传错 version
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({"name": "VC Co New", "version": 99})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "vc: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40901);
}

#[tokio::test]
async fn soft_delete_outsource_company_in_use_returns_21205() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "oc_inuse").await;
    let p1 = seed_outsource_process(&pool, "PROC-INUSE", "外协INUSE").await;
    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({
                "name": "InUse Co",
                "process_ids": [p1.to_string()]
            })),
            Some(&token),
        ),
    )
    .await;
    let cid = env_c["data"]["id"].as_str().unwrap().to_string();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "in-use: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21205);
}

#[tokio::test]
async fn list_outsource_companies_name_like_and_is_active_filter() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "oc_list").await;

    // 3 个公司：A 激活、B 激活、C 停用
    for (n, active) in [("AAAA Inc", true), ("BBBB Co", true), ("CCCC Ltd", false)] {
        let (s, _) = send(
            app.clone(),
            json_request(
                "POST",
                "/outsource-companies",
                Some(json!({"name": n, "is_active": active})),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }

    // name_like=AA → 1
    let (_, env1) = send(
        app.clone(),
        json_request("GET", "/outsource-companies?name_like=AA", None, Some(&token)),
    )
    .await;
    assert_eq!(env1["data"]["total"].as_i64().unwrap(), 1);

    // is_active=true → 2
    let (_, env2) = send(
        app.clone(),
        json_request("GET", "/outsource-companies?is_active=true", None, Some(&token)),
    )
    .await;
    assert_eq!(env2["data"]["total"].as_i64().unwrap(), 2);
}

#[tokio::test]
async fn set_outsource_company_processes_replaces_mapping() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "oc_setproc").await;
    let p1 = seed_outsource_process(&pool, "SP-1", "sp1").await;
    let p2 = seed_outsource_process(&pool, "SP-2", "sp2").await;
    let p3 = seed_outsource_process(&pool, "SP-3", "sp3").await;
    // 创建时只挂 p1
    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "SetProc Co", "process_ids": [p1.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    let cid = env_c["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(env_c["data"]["processes"].as_array().unwrap().len(), 1);

    // 整体替换为 [p2, p3]
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/processes"),
            Some(json!({"process_ids": [p2.to_string(), p3.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "set-processes: {env}");
    let procs = env["data"]["processes"].as_array().unwrap();
    assert_eq!(procs.len(), 2);
    let ids: Vec<String> = procs
        .iter()
        .map(|p| p["process_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&p2.to_string()));
    assert!(ids.contains(&p3.to_string()));
    // p1 应被清除
    assert!(!ids.contains(&p1.to_string()));
}

#[tokio::test]
async fn list_outsource_companies_by_process_filters_inactive() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "oc_byp").await;
    let p = seed_outsource_process(&pool, "BYP", "byp").await;
    // active
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "Active Co", "process_ids": [p.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    // inactive
    let (_, env_i) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "Inactive Co", "is_active": false, "process_ids": [p.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    let inactive_id = env_i["data"]["id"].as_str().unwrap().to_string();

    let (_, env) = send(
        app,
        json_request(
            "GET",
            &format!("/outsource-companies/by-process/{p}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let items = env["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_ne!(items[0]["id"].as_str().unwrap(), inactive_id);
}
