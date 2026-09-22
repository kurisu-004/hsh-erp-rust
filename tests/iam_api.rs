//! iam 域端到端集成测试（新路径 `/api/v2/iam/*`）
//!
//! 覆盖：
//! 1. /iam/login 成功 / 用户不存在 / 密码错 / 已停用 / 无角色
//! 2. /iam/me 成功 / 缺 Authorization
//! 3. /iam/refresh 成功 + 旧 token 二次使用被拒
//! 4. /iam/change-password 成功 + 旧 refresh 失效 / 旧密码错
//! 5. /iam/users CRUD 权限（无 MANAGER → 403）
//! 6. /iam/users/{id}/roles 添加 SHELF_ACCOUNT / 重复分配 409
//!
//! ## 测试并行注意
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立（每个用例 fresh database，无 fixture 覆盖 / 唯一约束撞车问题）。
//! 这是测试基建约束，不是产品代码约束——产品代码里每个 tx 都是原子的。
//!
//! 2026-09-19 IAM 域合并：从 `tests/auth_api.rs` 整体迁移过来，路径全改为
//! `/api/v2/iam/*`。原 `tests/auth_api_legacy.rs`（PR-1 兼容期回归 5 用例）随 PR-4
//! 旧 alias 下线一并删除。

#[path = "common/mod.rs"]
mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{
    add_role, add_role_menu, clean_db, clean_redis, ensure_database_exists,
    get_refresh_token_version, insert_inactive_user, insert_menu, insert_shelf,
    insert_user_with_password, test_app, test_pool, test_redis_pool, test_state,
};

// ===========================================================================
// 全局串行化互斥：所有测试共享同一 DB，必须串行访问避免 fixture 冲突。
// ===========================================================================

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

/// 走新路径 `/iam/login`
async fn login_admin(app: axum::Router, username: &str, password: &str) -> (StatusCode, Value) {
    send(
        app,
        json_request(
            "POST",
            "/iam/login",
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
async fn setup() -> sqlx::PgPool {
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    pool
}

// ===========================================================================
// 1. login (5)
// ===========================================================================

#[tokio::test]
async fn login_success_returns_token_pair_and_stamps_last_login() {
    let pool = setup().await;

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
    let pool = setup().await;
    let _ = pool;

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "ghost", "whatever").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_wrong_password_returns_40101() {
    let pool = setup().await;

    insert_user_with_password(&pool, "admin", "changeme").await;

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "admin", "wrong").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_inactive_user_returns_40101() {
    let pool = setup().await;

    insert_inactive_user(&pool, "admin", "changeme").await;

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "admin", "changeme").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_user_with_no_roles_returns_403_20606() {
    let pool = setup().await;

    insert_user_with_password(&pool, "lonely", "changeme").await;
    // 不插角色

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = login_admin(app, "lonely", "changeme").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(env["code"], 20606);
}

// ===========================================================================
// 2. me (2)
// ===========================================================================

#[tokio::test]
async fn me_success_returns_full_user_view() {
    let pool = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());

    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = test_app(state);
    let (status, env) = send(app2, json_request("GET", "/iam/me", None, Some(&token))).await;
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
    let pool = setup().await;
    let _ = pool;

    let state = test_state(pool).await;
    let app = test_app(state);

    let (status, env) = send(app, json_request("GET", "/iam/me", None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(env["code"], 0);
}

// ===========================================================================
// 3. refresh (2)
// ===========================================================================

#[tokio::test]
async fn refresh_rotates_token_and_bumps_version() {
    let pool = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());

    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let refresh_token = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    let ver_before = get_refresh_token_version(&pool, uid).await;
    assert_eq!(ver_before, 0, "新建用户 refresh_token_version=0");

    let app2 = test_app(state);
    let (status, env) = send(
        app2,
        json_request(
            "POST",
            "/iam/refresh",
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
async fn refresh_reusing_old_token_returns_40105() {
    // 2026-09-23 重构：reuse detection 接管"旧 refresh 二次使用"语义——
    // 第一次 refresh 时 `complete_refresh` 把旧 refresh jti 写黑名单（TTL 至 refresh_exp），
    // 第二次同 refresh 再调时 phase 1 `is_jti_revoked` 命中，40105 + force_logout，
    // 而不是 DB version check 的 40103。底层 DB 40103 路径仍然存在但被闸位抢答，
    // 详见 `refresh_reuse_detection_triggers_force_logout_and_40105` 端到端覆盖。
    let pool = setup().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());

    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let old_refresh = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // 第一次 refresh：成功
    let app2 = test_app(state.clone());
    let (s1, _) = send(
        app2,
        json_request(
            "POST",
            "/iam/refresh",
            Some(json!({"refresh_token": old_refresh.clone()})),
            None,
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // 第二次使用旧 refresh：黑名单命中 → SESSION_REVOKED（reuse detection）
    let app3 = test_app(state);
    let (status, env) = send(
        app3,
        json_request(
            "POST",
            "/iam/refresh",
            Some(json!({"refresh_token": old_refresh})),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        env["code"], 40105,
        "reuse detection 接管旧 refresh 二次使用 → 40105（不是 DB version 40103）: {env}"
    );
}

#[tokio::test]
async fn refresh_reuse_detection_triggers_force_logout_and_40105() {
    // 2026-09-23 重构：端到端覆盖 refresh rotation + reuse detection + force_logout 全链路。
    //
    // 关键断言：
    // 1. login → 拿 J1 (old_refresh)
    // 2. refresh(J1) 成功 → 拿 J2_access / J2_refresh（佐证 rotation 写新 session + 旧 jti 黑名单）
    // 3. GET /me 用 J2_access → 200（佐证 J2 已发新 session，旧 jti 黑名单不影响 J2）
    // 4. refresh(J1) 再来一次 → 40105（reuse detection 命中 + force_logout）
    // 5. GET /me 用 J2_access → 40105（佐证 force_logout 清空了 J2 用户的所有 session）
    //
    // 必须在 setup_with_redis 而非 setup() 下跑——`is_jti_revoked` 走 Redis 黑名单；
    // `test_state` 走 test_state_with_redis（建 redis_pool），`setup_with_redis` 调
    // `clean_redis` 保证 `session:tok:*` 与 `revoked:*` 都是空状态，避免上一个用例残留。
    let (pool, redis_pool) = setup_with_redis().await;

    let uid = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = common::test_state_with_redis(pool.clone(), redis_pool);
    let app = test_app(state.clone());

    // 1) login → J1 (old_refresh)
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let j1 = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // 2) refresh(J1) → J2_access / J2_refresh
    let app2 = test_app(state.clone());
    let (s1, refresh_env) = send(
        app2,
        json_request("POST", "/iam/refresh", Some(json!({"refresh_token": j1.clone()})), None),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "第一次 refresh 应成功: {refresh_env}");
    let j2_access = refresh_env["data"]["token"].as_str().unwrap().to_string();
    let j2_refresh = refresh_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(j1, j2_refresh, "新 refresh 必须与旧 refresh 不同（rotation 换了 jti）");

    // 3) GET /me 用 J2_access → 200
    let app3 = test_app(state.clone());
    let (s2, me_env) = send(app3, json_request("GET", "/iam/me", None, Some(&j2_access))).await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "新签发的 J2_access 必须立即可用（佐证 complete_refresh 已写 J2 session）: {me_env}"
    );

    // 4) refresh(J1) 再来一次 → 40105（reuse detection）
    let app4 = test_app(state.clone());
    let (s3, reuse_env) = send(
        app4,
        json_request("POST", "/iam/refresh", Some(json!({"refresh_token": j1})), None),
    )
    .await;
    assert_eq!(
        s3,
        StatusCode::UNAUTHORIZED,
        "reuse detection 命中 → 401: {reuse_env}"
    );
    assert_eq!(
        reuse_env["code"], 40105,
        "reuse detection 走 SESSION_REVOKED（不是 DB version 40103）: {reuse_env}"
    );

    // 5) GET /me 用 J2_access → 40105（force_logout 已清空所有 session）
    let app5 = test_app(state);
    let (s4, me_after_env) = send(
        app5,
        json_request("GET", "/iam/me", None, Some(&j2_access)),
    )
    .await;
    assert_eq!(
        s4,
        StatusCode::UNAUTHORIZED,
        "force_logout 必须让 J2_access 也失效 → 401: {me_after_env}"
    );
    assert_eq!(
        me_after_env["code"], 40105,
        "force_logout 后 /me 必须 40105（黑名单闸 + Redis 主条目都被清）: {me_after_env}"
    );
}

// ===========================================================================
// 4. change-password (2)
// ===========================================================================

#[tokio::test]
async fn change_password_invalidates_old_refresh_token() {
    let pool = setup().await;

    insert_user_with_password(&pool, "admin", "changeme").await;
    let uid = get_user_id(&pool, "admin").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());

    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let access_token = login_env["data"]["token"].as_str().unwrap().to_string();
    let old_refresh = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // 改密
    let app2 = test_app(state.clone());
    let (cp_status, cp_env) = send(
        app2,
        json_request(
            "POST",
            "/iam/change-password",
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
            "/iam/refresh",
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
    let pool = setup().await;

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
            "/iam/change-password",
            Some(json!({"old_password": "WRONG", "new_password": "newpass"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40104);
}

// ===========================================================================
// 5. /iam/users 权限 + 列表 (1)
// ===========================================================================

#[tokio::test]
async fn list_users_without_manager_role_returns_403() {
    let pool = setup().await;

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
    let (status, env) = send(
        app2,
        json_request("GET", "/iam/users", None, Some(&clerk_token)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(env["code"], 40300);
}

// ===========================================================================
// 6. /iam/users/{id}/roles (2)
// ===========================================================================

#[tokio::test]
async fn add_shelf_account_role_succeeds_for_manager() {
    let pool = setup().await;

    let admin_id = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, admin_id, "MANAGER", None, None).await;
    let target_id = insert_user_with_password(&pool, "shelfie", "changeme").await;
    let shelf_id = insert_shelf(&pool, "SH-A1", "A1 货架", "PRODUCTION").await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let admin_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = test_app(state);
    let uri = format!("/iam/users/{}/roles", target_id);
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
    let pool = setup().await;

    let admin_id = insert_user_with_password(&pool, "admin", "changeme").await;
    add_role(&pool, admin_id, "MANAGER", None, None).await;
    let target_id = insert_user_with_password(&pool, "shelfie", "changeme").await;
    let shelf_id = insert_shelf(&pool, "SH-B1", "B1 货架", "INSPECTION").await;

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let admin_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let uri = format!("/iam/users/{}/roles", target_id);
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
// Redis session 集成测试（PR-1 兼容期 IAM 域端到端测试）
// ===========================================================================

async fn setup_with_redis() -> (sqlx::PgPool, deadpool_redis::Pool) {

    ensure_database_exists().await;
    let pg_pool = test_pool().await;
    clean_db(&pg_pool).await;
    let redis_pool = test_redis_pool().await;
    clean_redis(&redis_pool).await;
    (pg_pool, redis_pool)
}

#[tokio::test]
async fn logout_kills_current_session() {
    let (pool, redis_pool) = setup_with_redis().await;

    insert_user_with_password(&pool, "admin", "changeme").await;
    let uid = get_user_id(&pool, "admin").await;
    add_role(&pool, uid, "MANAGER", None, None).await;

    let state = common::test_state_with_redis(pool.clone(), redis_pool);
    let app = test_app(state.clone());

    // 1) login → token A
    let (_, login_env) = login_admin(app, "admin", "changeme").await;
    let token_a = login_env["data"]["token"].as_str().unwrap().to_string();

    // 2) /iam/me(A) 200
    let app2 = test_app(state.clone());
    let (s, _) = send(app2, json_request("GET", "/iam/me", None, Some(&token_a))).await;
    assert_eq!(s, StatusCode::OK, "login 后 /iam/me 必须 200");

    // 3) logout(A) 200
    let app3 = test_app(state.clone());
    let (lo_status, lo_env) = send(
        app3,
        json_request("POST", "/iam/logout", None, Some(&token_a)),
    )
    .await;
    assert_eq!(lo_status, StatusCode::OK, "logout 200: {lo_env}");

    // 4) /iam/me(A) → 40105（SESSION_REVOKED）
    let app4 = test_app(state);
    let (status, env) = send(app4, json_request("GET", "/iam/me", None, Some(&token_a))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "logout 后 /iam/me 必须 401: {env}"
    );
    assert_eq!(env["code"], 40105, "必须是 SESSION_REVOKED: {env}");
}

// ===========================================================================
// Silence unused imports when a single test compiles but the others don't.
// ===========================================================================
#[allow(dead_code)]
fn _unused_silencer() {
    let _ = insert_menu;
    let _ = add_role_menu;
}
