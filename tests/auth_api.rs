//! auth + user 域端到端集成测试
//!
//! 覆盖：
//! 1. /auth/login 成功 / 用户不存在 / 密码错 / 已停用 / 无角色
//! 2. /auth/me 成功 / 缺 Authorization
//! 3. /auth/refresh 成功 + 旧 token 二次使用被拒
//! 4. /auth/change-password 成功 + 旧 refresh 失效 / 旧密码错
//! 5. /users CRUD 权限（无 MANAGER → 403）
//! 6. /users/{id}/roles 添加 SHELF_ACCOUNT / 重复分配 409
//!
//! ## 测试并行注意
//! 所有用例共享同一个 `postgres_rust_test` 库 + `t_user.username` 唯一索引（partial）。
//! 多个 `#[tokio::test]` 并发跑时会相互覆盖 fixture，撞唯一约束。
//! 用一个进程级 `tokio::sync::Mutex` 在每个用例入口序列化对 DB 的写入/截断。
//! 这是测试基建约束，不是产品代码约束——产品代码里每个 tx 都是原子的。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use common::{
    add_role, add_role_menu, clean_db, clean_redis, ensure_database_exists,
    get_refresh_token_version, insert_inactive_user, insert_menu, insert_shelf,
    insert_user_with_password, test_app, test_pool, test_redis_pool, test_state,
};

// ===========================================================================
// 全局串行化互斥：所有测试共享同一 DB，必须串行访问避免 fixture 冲突。
// ===========================================================================
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 取单测用的 username → 在 DB 中回查 user_id
async fn get_user_id(pool: &sqlx::PgPool, username: &str) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM t_user WHERE username = $1"#,
        username.to_lowercase()
    )
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("回查用户 id 失败 ({username}): {e}"))
}

// ===========================================================================
// Helpers
// ===========================================================================

/// 把 request 发给 axum app，oneshot 出来，拆 (status, body JSON envelope)。
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

async fn login_admin(app: axum::Router, username: &str, password: &str) -> (StatusCode, Value) {
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

/// 用例开头固定三步：拿到锁 → 建库 → 连池 + 迁移 → 清表。
///
/// **重要**：返回的 `MutexGuard` 必须绑到 `_guard` 一直活到用例结束，否则锁在
/// `setup()` 返回时立刻释放，后续用例会并发跑、相互覆盖 fixture。
async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, sqlx::PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    (guard, pool)
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test]
async fn login_success_returns_token_pair_and_stamps_last_login() {
    let (_guard, pool) = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "admin", "changeme").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env["code"], 0, "envelope.code = 0; full = {env}");

    let data = &env["data"];
    assert!(data["token"].as_str().unwrap().len() > 20);
    assert!(data["refresh_token"].as_str().unwrap().len() > 20);
    assert_eq!(data["user"]["username"], "admin");
    assert_eq!(data["user"]["roles"][0], "MANAGER");
    // id 序列化为字符串
    let id_str = data["user"]["id"].as_str().expect("user.id 是字符串");
    assert_eq!(id_str.parse::<i64>().unwrap(), uid);

    // DB 中 last_login_at 非空
    let row = sqlx::query!(
        "SELECT last_login_at AS \"last?\" FROM t_user WHERE id = $1",
        uid
    )
    .fetch_one(&pool)
    .await
    .expect("query last_login_at");
    assert!(row.last.is_some(), "last_login_at 应在登录后被写入");
}

#[tokio::test]
async fn login_unknown_user_returns_40101() {
    let (_guard, pool) = setup().await;
    let _ = pool;

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "ghost", "whatever").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_wrong_password_returns_40101() {
    let (_guard, pool) = setup().await;

    insert_user_with_password(&pool, "admin", "changeme").await;

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "admin", "wrong").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_inactive_user_returns_40101() {
    let (_guard, pool) = setup().await;

    insert_inactive_user(&pool, "admin", "changeme").await;

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "admin", "changeme").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_user_with_no_roles_returns_403_20606() {
    let (_guard, pool) = setup().await;

    insert_user_with_password(&pool, "lonely", "changeme").await;
    // 不插角色

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "lonely", "changeme").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(env["code"], 20606);
}

#[tokio::test]
async fn me_success_returns_full_user_view() {
    let (_guard, pool) = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());

    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = test_app(state);
    let (status, env) = send(
        app2,
        json_request("GET", "/auth/me", None, Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["username"], "admin");
    assert_eq!(env["data"]["roles"][0], "MANAGER");
    assert_eq!(
        env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap(),
        uid
    );
}

#[tokio::test]
async fn me_without_authorization_returns_401() {
    let (_guard, pool) = setup().await;
    let _ = pool;

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = send(app, json_request("GET", "/auth/me", None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(env["code"], 0);
}

#[tokio::test]
async fn refresh_rotates_token_and_bumps_version() {
    let (_guard, pool) = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());

    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let refresh_token = login_env["data"]["refresh_token"].as_str().unwrap().to_string();
    let ver_before = get_refresh_token_version(&pool, uid).await;
    assert_eq!(ver_before, 0, "新建用户 refresh_token_version=0");

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

    let ver_after = get_refresh_token_version(&pool, uid).await;
    assert_eq!(ver_after, 1, "refresh 后 version 应当 +1");
}

#[tokio::test]
async fn refresh_reusing_old_token_returns_40103() {
    let (_guard, pool) = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());

    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let old_refresh = login_env["data"]["refresh_token"].as_str().unwrap().to_string();

    // 第一次 refresh：成功
    let app2 = test_app(state.clone());
    let (s1, _) = send(
        app2,
        json_request(
            "POST",
            "/auth/refresh",
            Some(json!({"refresh_token": old_refresh.clone()})),
            None,
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // 第二次使用旧 refresh：版本已轮转 → REFRESH_INVALID
    let app3 = test_app(state);
    let (status, env) = send(
        app3,
        json_request(
            "POST",
            "/auth/refresh",
            Some(json!({"refresh_token": old_refresh})),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40103);
}

#[tokio::test]
async fn change_password_invalidates_old_refresh_token() {
    let (_guard, pool) = setup().await;

    insert_user_with_password(&pool, "admin", "changeme").await;
    let uid = get_user_id(&pool, "admin").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());

    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let access_token = login_env["data"]["token"].as_str().unwrap().to_string();
    let old_refresh = login_env["data"]["refresh_token"].as_str().unwrap().to_string();

    // 改密
    let app2 = test_app(state.clone());
    let (cp_status, cp_env) = send(
        app2,
        json_request(
            "POST",
            "/auth/change-password",
            Some(json!({"old_password": "changeme", "new_password": "newpass"})),
            Some(&access_token),
        ),
    )
    .await;
    assert_eq!(cp_status, StatusCode::OK, "change-password: {cp_env}");

    // 旧 refresh 失效
    let app3 = test_app(state);
    let (status, env) = send(
        app3,
        json_request(
            "POST",
            "/auth/refresh",
            Some(json!({"refresh_token": old_refresh})),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40103);
}

#[tokio::test]
async fn change_password_wrong_old_password_returns_40104() {
    let (_guard, pool) = setup().await;

    insert_user_with_password(&pool, "admin", "changeme").await;
    let uid = get_user_id(&pool, "admin").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = test_app(state);
    let (status, env) = send(
        app2,
        json_request(
            "POST",
            "/auth/change-password",
            Some(json!({"old_password": "WRONG", "new_password": "newpass"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40104);
}

#[tokio::test]
async fn list_users_without_manager_role_returns_403() {
    let (_guard, pool) = setup().await;

    // 创建 admin（MANAGER） + 普通 user（CLERK）
    let admin_id = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, admin_id, "MANAGER", None, None).await;
    let clerk_id = insert_user_with_password(&pool, "clerk", "changeme").await;
    add_role(&pool, clerk_id, "CLERK", None, None).await;

    let state = test_state(pool).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_admin(app, "clerk", "changeme").await;
    let clerk_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = test_app(state);
    let (status, env) = send(app2, json_request("GET", "/users", None, Some(&clerk_token))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(env["code"], 40300);
}

#[tokio::test]
async fn add_shelf_account_role_succeeds_for_manager() {
    let (_guard, pool) = setup().await;

    let admin_id = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, admin_id, "MANAGER", None, None).await;
    let target_id = insert_user_with_password(&pool, "shelfie", "changeme").await;
    let shelf_id = insert_shelf(&pool, "SH-A1", "A1 货架", "PRODUCTION").await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
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

#[tokio::test]
async fn add_duplicate_role_returns_409() {
    let (_guard, pool) = setup().await;

    let admin_id = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, admin_id, "MANAGER", None, None).await;
    let target_id = insert_user_with_password(&pool, "shelfie", "changeme").await;
    let shelf_id = insert_shelf(&pool, "SH-B1", "B1 货架", "INSPECTION").await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let admin_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let uri = format!("/users/{}/roles", target_id);
    let body = json!({
        "role": "SHELF_ACCOUNT",
        "scope_type": "shelf",
        "scope_id": shelf_id,
    });

    // 第一次：成功
    let app2 = test_app(state.clone());
    let (s1, _) = send(
        app2,
        json_request("POST", &uri, Some(body.clone()), Some(&admin_token)),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    // 第二次：重复 → 409
    let app3 = test_app(state);
    let (status, env) = send(
        app3,
        json_request("POST", &uri, Some(body), Some(&admin_token)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(env["code"], 20604);
}

// ===========================================================================
// Redis session 集成测试（方案：redis-session-deadpool-redis-0-23）
//
// 3 个 case 共享 setup 形态：ensure_db → 池 + 迁移 → clean_db → clean_redis → test_state。
// TEST_LOCK 串行避免 fixture 冲突（与既有测试同模式）。
// ===========================================================================

/// 扩展 setup：除 DB 外再 FLUSHDB 测试 Redis db 15，保证 session 状态干净。
async fn setup_with_redis<'a>()
-> (tokio::sync::MutexGuard<'a, ()>, sqlx::PgPool, deadpool_redis::Pool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pg_pool = test_pool().await;
    clean_db(&pg_pool).await;
    let redis_pool = test_redis_pool().await;
    clean_redis(&redis_pool).await;
    (guard, pg_pool, redis_pool)
}

#[tokio::test]
async fn logout_kills_current_session() {
    let (_guard, pool, redis_pool) = setup_with_redis().await;

    insert_user_with_password(&pool, "admin", "changeme").await;
    let uid = get_user_id(&pool, "admin").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = common::test_state_with_redis(pool.clone(), redis_pool);
    let app = test_app(state.clone());

    // 1) login → token A
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let token_a = login_env["data"]["token"].as_str().unwrap().to_string();

    // 2) /me(A) 200
    let app2 = test_app(state.clone());
    let (s, _) = send(app2, json_request("GET", "/auth/me", None, Some(&token_a))).await;
    assert_eq!(s, StatusCode::OK, "login 后 /me 必须 200");

    // 3) logout(A) 200
    let app3 = test_app(state.clone());
    let (lo_status, lo_env) = send(app3, json_request("POST", "/auth/logout", None, Some(&token_a))).await;
    assert_eq!(lo_status, StatusCode::OK, "logout 200: {lo_env}");

    // 4) /me(A) → 40105（SESSION_REVOKED）
    let app4 = test_app(state);
    let (status, env) = send(app4, json_request("GET", "/auth/me", None, Some(&token_a))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "logout 后 /me 必须 401: {env}");
    assert_eq!(env["code"], 40105, "必须是 SESSION_REVOKED: {env}");
}

#[tokio::test]
async fn change_password_kills_all_sessions() {
    let (_guard, pool, redis_pool) = setup_with_redis().await;

    insert_user_with_password(&pool, "admin", "changeme").await;
    let uid = get_user_id(&pool, "admin").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = common::test_state_with_redis(pool.clone(), redis_pool);
    let app = test_app(state.clone());

    // 1) 同账号两次登录拿 token A、B
    let (_, login_a) = login_admin(app, "admin", "changeme").await;
    let token_a = login_a["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state.clone());
    let (_, login_b) = login_admin(app2, "admin", "changeme").await;
    let token_b = login_b["data"]["token"].as_str().unwrap().to_string();
    assert_ne!(token_a, token_b, "两次登录应生成不同 token");

    // 2) /me(A) 200、/me(B) 200
    let app3 = test_app(state.clone());
    let (s_a, _) = send(app3, json_request("GET", "/auth/me", None, Some(&token_a))).await;
    assert_eq!(s_a, StatusCode::OK);
    let app4 = test_app(state.clone());
    let (s_b, _) = send(app4, json_request("GET", "/auth/me", None, Some(&token_b))).await;
    assert_eq!(s_b, StatusCode::OK);

    // 3) 用 A 改密
    let app5 = test_app(state.clone());
    let (cp_status, cp_env) = send(
        app5,
        json_request(
            "POST",
            "/auth/change-password",
            Some(json!({"old_password": "changeme", "new_password": "newpass"})),
            Some(&token_a),
        ),
    )
    .await;
    assert_eq!(cp_status, StatusCode::OK, "change-password: {cp_env}");

    // 4) /me(A) → 40105、/me(B) → 40105（双 session 全清）
    let app6 = test_app(state.clone());
    let (s1, e1) = send(app6, json_request("GET", "/auth/me", None, Some(&token_a))).await;
    assert_eq!(s1, StatusCode::UNAUTHORIZED);
    assert_eq!(e1["code"], 40105, "A 应被吊销: {e1}");

    let app7 = test_app(state);
    let (s2, e2) = send(app7, json_request("GET", "/auth/me", None, Some(&token_b))).await;
    assert_eq!(s2, StatusCode::UNAUTHORIZED);
    assert_eq!(e2["code"], 40105, "B 应被吊销: {e2}");
}

#[tokio::test]
async fn refresh_transitions_old_session_to_new() {
    let (_guard, pool, redis_pool) = setup_with_redis().await;

    insert_user_with_password(&pool, "admin", "changeme").await;
    let uid = get_user_id(&pool, "admin").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = common::test_state_with_redis(pool.clone(), redis_pool);
    let app = test_app(state.clone());

    // 1) login → A (access) + R (refresh)
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let token_a = login_env["data"]["token"].as_str().unwrap().to_string();
    let refresh_r = login_env["data"]["refresh_token"].as_str().unwrap().to_string();

    // 2) /me(A) 200
    let app2 = test_app(state.clone());
    let (s, _) = send(app2, json_request("GET", "/auth/me", None, Some(&token_a))).await;
    assert_eq!(s, StatusCode::OK);

    // 3) refresh(R) → 新 pair (A2, R2)
    let app3 = test_app(state.clone());
    let (ref_status, ref_env) = send(
        app3,
        json_request(
            "POST",
            "/auth/refresh",
            Some(json!({"refresh_token": refresh_r.clone()})),
            None,
        ),
    )
    .await;
    assert_eq!(ref_status, StatusCode::OK, "refresh 成功: {ref_env}");
    let token_a2 = ref_env["data"]["token"].as_str().unwrap().to_string();
    let refresh_r2 = ref_env["data"]["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(token_a2, token_a);
    assert_ne!(refresh_r2, refresh_r);

    // 4) /me(A2) 200（新 access 立即可用）
    let app4 = test_app(state.clone());
    let (s2, _) = send(app4, json_request("GET", "/auth/me", None, Some(&token_a2))).await;
    assert_eq!(s2, StatusCode::OK, "新 access token 必须立即可用");

    // 5) 旧 refresh(R) 再用 → 40103（已被 Redis 删，且 DB version 也轮转了）
    let app5 = test_app(state.clone());
    let (s_old_ref, env_old_ref) = send(
        app5,
        json_request(
            "POST",
            "/auth/refresh",
            Some(json!({"refresh_token": refresh_r})),
            None,
        ),
    )
    .await;
    assert_eq!(s_old_ref, StatusCode::UNAUTHORIZED, "旧 refresh 必失效");
    assert_eq!(env_old_ref["code"], 40103, "旧 refresh → 40103: {env_old_ref}");

    // 6) 旧 access(A) 仍有效（refresh 只删 refresh 条目；access 走自然过期）
    let app6 = test_app(state);
    let (s_old_a, env_old_a) = send(app6, json_request("GET", "/auth/me", None, Some(&token_a))).await;
    assert_eq!(s_old_a, StatusCode::OK, "旧 access 在自然到期前仍可用: {env_old_a}");
}

// ===========================================================================
// Session check toggle（REDIS_SESSION_CHECK_ENABLED=false）集成测试
//
// 场景：Rust 后端在迁移早期会借 Python myERP 签发的 JWT；此时 Redis 服务端
// session 校验必须关闭（否则 40105 会把全部 Python 签的 access token 全部
// 拒掉）。本测试不依赖 Redis 容器，验证：
//   1) AppState 的 session store 是 NoopSessionStore
//   2) 手签一个 Python 形态 JWT（角色是 Role::Clerk，不是 MANAGER —— 模拟 Python
//      普通用户），extractor 不查 Redis，直接从 claims 构造 CurrentUser
//   3) /auth/me 200，证明 extractor 跳过了 Redis 查询（走 claims 路径）
//   4) hash_token 派生函数仍工作（保证 Noop 路径不破坏 extract 链）
// ===========================================================================

/// session check 关闭时：手签一个「类 Python 形态」的 JWT（不在 Redis 写入任何条目），
/// `/auth/me` 应返回 200，证明 extractor 跳过了 Redis 查询（直接走 claims 路径）。
/// 注意：`me` handler 会重读 DB 取完整用户信息，所以此测试只断言 200 status，
/// 不验证 JWT claims 中的 roles/shelf_ids —— 那些字段的精确路径
/// 由 unit test 直接构造 CurrentUser 验证（不在本集成测试范围）。
#[tokio::test]
async fn disabled_session_accepts_python_issued_jwt() {
    use chrono::Utc;
    use hsh_erp_rust::auth::jwt::encode_access;
    use hsh_erp_rust::auth::rbac::{Claims, Role};
    use hsh_erp_rust::auth::session::hash_token;

    // 用例不依赖 Redis 容器（NoopSessionStore 跳过 Redis）；
    // 仍走标准 setup()，与其他测试一样持有 TEST_LOCK，避免并发清表。
    let (_guard, pool) = setup().await;

    // /me handler 会按 CurrentUser.id 回查 t_user；插一个 active 用户（不插 role，
    // 因为 extractor 已从 JWT claims 直接构造 CurrentUser，service 层 /me 只读最新
    // 状态，不再依赖 Redis 缓存里的角色）。
    let user_id = insert_user_with_password(&pool, "python_user", "any-password").await;

    let state = common::test_state_with_disabled_session(pool.clone());

    // 手签一个「Python 形态」JWT：roles 用全大写字符串、shelf_wildcard 默认 false
    let claims = Claims {
        sub: user_id,
        username: "python_user".to_string(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
        ver: 0,
        typ: "access".into(),
        iss: state.config.jwt.issuer.clone(),
        exp: Utc::now().timestamp() + 3600,
    };
    let (token, _exp) = encode_access(
        &claims,
        &state.config.jwt.secret,
        &state.config.jwt.issuer,
        state.config.jwt.access_ttl_hours,
    )
    .expect("encode access token");

    // 确认 Redis 没有写入（NoopSessionStore::create_session 不会被调用，因为根本没走 login）
    // 直接打 /me：extractor 跳过 Redis 查询，从 claims 构造 CurrentUser
    let app = test_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/auth/me")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "session check 关闭时手签 JWT 应被接受；当前 status = {}",
        resp.status()
    );

    // 顺便断言 hash_token 派生函数仍工作（保证 Noop 路径不破坏 extract 链）
    assert_eq!(hash_token(&token).len(), 64);
}

// ===========================================================================
// Silence unused imports when a single test compiles but the others don't.
// ===========================================================================
#[allow(dead_code)]
fn _unused_silencer() {
    let _ = insert_menu;
    let _ = add_role_menu;
}
