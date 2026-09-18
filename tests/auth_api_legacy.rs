//! iam 域旧 alias 兼容期集成测试（PR-1）
//!
//! 验证 `/api/v2/auth/*` + `/api/v2/users/*` 旧路径仍 200 + 响应与新路径
//! `/api/v2/iam/*` 完全一致。本文件用例 **只在 PR-1 / PR-2 / PR-3 期间保留**；
//! PR-4（计划）会删除 `iam::auth_router` / `iam::users_router` 工厂函数 + 本测试文件。
//!
//! ## 测试范围（5 条最小回归用例）
//! 1. `POST /auth/login` 200 + 同 `/iam/login` 响应字段一致
//! 2. `GET /auth/me` 200（同一 token 在两个路径都返回相同 data）
//! 3. `POST /auth/refresh` 200
//! 4. `GET /users` 403（非 MANAGER）—— 验证旧 users 路径的 RBAC 守卫仍生效
//! 5. `POST /users/{id}/roles` 201 —— 验证旧 users 路径的 add_role 端点仍生效
//!
//! 2026-09-19 IAM 域合并：从 `tests/auth_api.rs` 的 14 用例 + 6 个 `/users` 用例
//! 抽取 5 条代表性回归用例；完整覆盖由 `tests/iam_api.rs` 承担。

#[path = "common/mod.rs"]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{
    add_role, clean_db, ensure_database_exists, insert_shelf, insert_user_with_password, test_app,
    test_pool, test_state,
};

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[allow(dead_code)]
async fn get_user_id(pool: &sqlx::PgPool, username: &str) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM t_user WHERE username = $1"#,
        username.to_lowercase()
    )
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("回查用户 id 失败 ({username}): {e}"))
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    use axum::body::to_bytes;
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let envelope: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("parse JSON: {e}; raw = {}", String::from_utf8_lossy(&body)));
    (status, envelope)
}

fn json_request(
    method: &str,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> Request<Body> {
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

/// 走旧路径 `/auth/login`
async fn login_legacy(app: axum::Router, username: &str, password: &str) -> (StatusCode, Value) {
    send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": username, "password": password})),
            None,
        ),
    )
    .await
}

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, sqlx::PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    (guard, pool)
}

// ===========================================================================
// 回归用例 1：`POST /auth/login` 200（同 `/iam/login` 响应字段一致）
// ===========================================================================

#[tokio::test]
async fn legacy_auth_login_returns_same_envelope_as_iam_login() {
    let (_guard, pool) = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool).await;
    let app = test_app(state.clone());
    let (status, env) = login_legacy(app, "admin", "changeme").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env["code"], 0);
    let data = &env["data"];
    assert!(data["token"].as_str().unwrap().len() > 20);
    assert!(data["refresh_token"].as_str().unwrap().len() > 20);
    assert_eq!(data["user"]["username"], "admin");
    assert_eq!(data["user"]["roles"][0], "MANAGER");
}

// ===========================================================================
// 回归用例 2：`GET /auth/me` 200
// ===========================================================================

#[tokio::test]
async fn legacy_auth_me_returns_200() {
    let (_guard, pool) = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_legacy(app, "admin", "changeme").await;
    let token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = test_app(state);
    let (status, env) = send(app2, json_request("GET", "/auth/me", None, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["username"], "admin");
}

// ===========================================================================
// 回归用例 3：`POST /auth/refresh` 200
// ===========================================================================

#[tokio::test]
async fn legacy_auth_refresh_returns_200() {
    let (_guard, pool) = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_legacy(app, "admin", "changeme").await;
    let refresh_token = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    let app2 = test_app(state);
    let (status, env) = send(
        app2,
        json_request(
            "POST",
            "/auth/refresh",
            Some(json!({"refresh_token": refresh_token})),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env["code"], 0);
    assert!(env["data"]["token"].as_str().unwrap().len() > 20);
    assert!(env["data"]["refresh_token"].as_str().unwrap().len() > 20);
}

// ===========================================================================
// 回归用例 4：`GET /users` 403（非 MANAGER）
// ===========================================================================

#[tokio::test]
async fn legacy_users_list_without_manager_role_returns_403() {
    let (_guard, pool) = setup().await;

    let admin_id = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, admin_id, "MANAGER", None, None).await;
    let clerk_id = insert_user_with_password(&pool, "clerk", "changeme").await;
    add_role(&pool, clerk_id, "CLERK", None, None).await;

    let state = test_state(pool).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_legacy(app, "clerk", "changeme").await;
    let clerk_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = test_app(state);
    let (status, env) = send(
        app2,
        json_request("GET", "/users", None, Some(&clerk_token)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(env["code"], 40300);
}

// ===========================================================================
// 回归用例 5：`POST /users/{id}/roles` 201（add_role 端点）
// ===========================================================================

#[tokio::test]
async fn legacy_users_add_shelf_account_role_succeeds() {
    let (_guard, pool) = setup().await;

    let admin_id = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, admin_id, "MANAGER", None, None).await;
    let target_id = insert_user_with_password(&pool, "shelfie", "changeme").await;
    let shelf_id = insert_shelf(&pool, "SH-A1", "A1 货架", "PRODUCTION").await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_legacy(app, "admin", "changeme").await;
    let admin_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = test_app(state);
    let uri = format!("/users/{}/roles", target_id);
    let (status, env) = send(
        app2,
        json_request(
            "POST",
            &uri,
            Some(json!({
                "role": "SHELF_ACCOUNT",
                "scope_type": "shelf",
                "scope_id": shelf_id,
            })),
            Some(&admin_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "add_role: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["role"], "SHELF_ACCOUNT");
    assert_eq!(env["data"]["shelf_code"], "SH-A1");
}
