//! delivery-group 端到端集成测试
//!
//! Phase P1 覆盖：
//! 1. create group 成功（200）→ list_for_l1 返回新 group + members
//! 2. 同 L1 重名 → 409 / 21414
//! 3. 成员 L2 已属他组 → 409 / 21415
//! 4. update members（替换）→ 200；旧成员消失、新成员就位
//! 5. version conflict → 409 / VERSION_CONFLICT
//! 6. soft-delete → 200；后续 list 排除该组
//! 7. customer_id 不是 L1 → 400 / 20104
//! 8. customer_id 不存在 → 404 / 20102
//!
//! ## 并行
//! 所有用例共享 `postgres_rust_test` + 唯一约束 (`uq_t_delivery_group_name_active`、
//! `uq_t_customer_root_prefix`)，用进程级 `tokio::sync::Mutex` 串行化。
//!
//! ## 认证
//! 每个用例都创建一个 MANAGER 用户、登录拿 token。所有 POST /delivery-groups/*
//   都要求 M/C，按设计 §6.1 用 MANAGER 跑通即可。

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

/// 建一个 MANAGER 用户并登录，返回 access token + state
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

/// 直插 L1 客户（绕开 customer 域 CRUD）
async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
        1_577_836_800_000,
        1,
    );
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id,
        name,
        prefix,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

/// 直插 L2 客户（parent_id = l1_id）
async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
        1_577_836_800_000,
        1,
    );
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
    .expect("insert L2");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_group_succeeds_and_appears_in_list() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_a = insert_l2(&pool, "二厂", l1).await;
    let l2_b = insert_l2(&pool, "五厂", l1).await;

    let (app, token) = login_manager(pool, "admin").await;

    // create
    let (status, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "二五六厂",
                "member_customer_ids": [l2_a.to_string(), l2_b.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["name"], "二五六厂");
    assert_eq!(env["data"]["members"].as_array().unwrap().len(), 2);

    // list
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            &format!("/delivery-groups?customer_id={l1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    let groups = env2["data"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["name"], "二五六厂");
    assert_eq!(groups[0]["members"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn create_duplicate_name_returns_409_21414() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    insert_l2(&pool, "二厂", l1).await;

    let (app, token) = login_manager(pool, "admin").await;

    // 第一次
    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "二五六厂",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // 第二次同 L1 同名
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "二五六厂",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(env2["code"], 21414, "code = 21414; full = {env2}");
}

#[tokio::test]
async fn create_with_member_in_other_group_returns_409_21415() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_a = insert_l2(&pool, "二厂", l1).await;
    let l2_b = insert_l2(&pool, "五厂", l1).await;

    let (app, token) = login_manager(pool, "admin").await;

    // 创建第一个组（成员 = 二厂）
    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "组 A",
                "member_customer_ids": [l2_a.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // 创建第二个组，成员包括已被组 A 占用的 l2_a → 409 / 21415
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "组 B",
                "member_customer_ids": [l2_b.to_string(), l2_a.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(env2["code"], 21415, "code = 21415; full = {env2}");
}

#[tokio::test]
async fn update_members_full_replace_succeeds() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_a = insert_l2(&pool, "二厂", l1).await;
    let l2_b = insert_l2(&pool, "五厂", l1).await;
    let l2_c = insert_l2(&pool, "六厂", l1).await;

    let (app, token) = login_manager(pool, "admin").await;

    // create (members = [二厂])
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "动态组",
                "member_customer_ids": [l2_a.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let gid = env1["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();
    let v0 = env1["data"]["version"].as_i64().unwrap() as i32;

    // update members = [五厂, 六厂]（全量替换）
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-groups/{gid}/update"),
            Some(json!({
                "version": v0,
                "member_customer_ids": [l2_b.to_string(), l2_c.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "update: {env2}");
    let members_after: Vec<i64> = env2["data"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["customer_id"].as_str().unwrap().parse::<i64>().unwrap())
        .collect();
    let mut expected = vec![l2_b, l2_c];
    expected.sort();
    let mut actual = members_after.clone();
    actual.sort();
    assert_eq!(actual, expected, "members 全量替换");
}

#[tokio::test]
async fn update_with_wrong_version_returns_409_version_conflict() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;

    let (app, token) = login_manager(pool, "admin").await;

    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "G1",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let gid = env1["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();

    // 用错误 version 调 update → 409 / VERSION_CONFLICT
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-groups/{gid}/update"),
            Some(json!({
                "version": 9999,
                "name": "G1-renamed",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(env2["code"], 40901);
}

#[tokio::test]
async fn soft_delete_removes_group_from_list() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;

    let (app, token) = login_manager(pool, "admin").await;

    // create
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "to-delete",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let gid = env1["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();
    let v0 = env1["data"]["version"].as_i64().unwrap() as i32;

    // soft-delete
    let (s2, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-groups/{gid}/soft-delete"),
            Some(json!({"version": v0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);

    // list 不再包含 to-delete
    let (_, env3) = send(
        app,
        json_request(
            "GET",
            &format!("/delivery-groups?customer_id={l1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let groups = env3["data"]["groups"].as_array().unwrap();
    assert!(
        groups.iter().all(|g| g["name"] != "to-delete"),
        "soft-deleted group 不应在 list 里"
    );
}

#[tokio::test]
async fn create_with_non_l1_customer_returns_400_20104() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await; // L2 customer

    let (app, token) = login_manager(pool, "admin").await;

    // 把 L2 作为 customer_id 提交 → 400 / 20104
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/delivery-groups",
            Some(json!({
                "customer_id": l2.to_string(),
                "name": "非法",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(env["code"], 20104, "code = 20104 (BIZ_INVALID_VALUE); full = {env}");
}

#[tokio::test]
async fn list_with_nonexistent_customer_returns_404_20102() {
    let (_guard, pool) = setup().await;

    let (app, token) = login_manager(pool, "admin").await;

    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/delivery-groups?customer_id=999999999",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(env["code"], 20102);
}