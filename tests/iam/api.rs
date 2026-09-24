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
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原重度依赖 `test-support::fixtures::insert_user_with_password /
//! add_role / insert_inactive_user / insert_shelf / get_refresh_token_version`，
//! 是 fixtures.rs 在 iam 域的最后重度用户。本 commit 改走
//! `use hsh_erp_test_support::*` + `load_iam_fixture(&pool)` + `IamFixture` +
//! `bootstrap_as_*` 样板，所有 fixtures.rs 调用归零（Gate 5 验证）。
//!
//! 保留本地 helper：
//! - `login_admin` —— 走 `/iam/login` 拿完整信封（用于断言 `data.user` /
//!   `data.refresh_token` 等字段；`login_token` 只返 token 字符串）

use axum::body::Body;
use axum::http::{Request, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token,
    send as ts_send, test_app, test_pool, test_state,
};

// ===========================================================================
// Helpers
// ===========================================================================

/// 把 request 发给 axum app，oneshot 出来，拆 (status, body JSON envelope)。
///
/// 2026-09-24 PR13 Phase I：本文件原 `send` 与 `test-support::http::send`
/// 逐字一致，通过 `use ... send as ts_send` 别名复用，避免重复实现
///（Phase F 已收敛 27+ 副本，本文件是第 28 集）。
async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    ts_send(app, req).await
}

/// 走新路径 `/iam/login` 拿完整信封（保留为本地 helper：login_token 只返
/// token 字符串，本 helper 保留是为了「login_success」等需要检查
/// `data.user.username / data.user.id / data.refresh_token` 等字段的场景）。
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

/// 基础 bootstrap：fresh DB + iam fixture 9 行 + state + app。
///
/// 不登录；适合 login_unknown_user / me_without_authorization 等"无前置
/// 登录"测试的基底。fixture 数据不参与这些测试断言，但加载过程本身无害。
async fn bootstrap() -> (PgPool, axum::Router, IamFixture) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state);
    (pool, app, fx)
}

/// 构造一个全新的 Router（一次性 oneshot 后原 Router 被消耗，要多次发请求
/// 必须每个请求都新构造；这是 axum 0.8 oneshot 的语义）。
async fn fresh_app(pool: &PgPool) -> axum::Router {
    test_app(test_state(pool.clone()).await)
}

// ===========================================================================
// 1. login (5)
// ===========================================================================

#[tokio::test]
async fn login_success_returns_token_pair_and_stamps_last_login() {
    let (pool, app, fx) = bootstrap().await;

    let (_, env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    assert_eq!(env["code"], 0, "envelope.code = 0; full = {env}");

    let data = &env["data"];
    assert!(data["token"].as_str().unwrap().len() > 20);
    assert!(data["refresh_token"].as_str().unwrap().len() > 20);
    assert_eq!(data["user"]["username"], fx.manager_username);
    assert_eq!(data["user"]["roles"][0], "MANAGER");
    // id 序列化为字符串
    let id_str = data["user"]["id"].as_str().expect("user.id 是字符串");
    assert_eq!(id_str.parse::<i64>().unwrap(), fx.manager_user_id);

    // DB 中 last_login_at 非空
    let row = sqlx::query!(
        "SELECT last_login_at AS \"last?\" FROM t_user WHERE id = $1",
        fx.manager_user_id
    )
    .fetch_one(&pool)
    .await
    .expect("query last_login_at");
    assert!(row.last.is_some(), "last_login_at 应在登录后被写入");
}

#[tokio::test]
async fn login_unknown_user_returns_40101() {
    let (_pool, app, _fx) = bootstrap().await;
    // fixture 数据不影响此测试：login("ghost") 命中"用户不存在"分支，
    // 不论 fixture 内是否有用户。
    let (status, env) = login_admin(app, "ghost", "whatever").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_wrong_password_returns_40101() {
    let (_pool, app, fx) = bootstrap().await;

    let (status, env) =
        login_admin(app, &fx.manager_username, "wrong").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_inactive_user_returns_40101() {
    let (_pool, app, fx) = bootstrap().await;

    let (status, env) =
        login_admin(app, &fx.inactive_username, IamFixture::PASSWORD).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(env["code"], 40101);
}

#[tokio::test]
async fn login_user_with_no_roles_returns_403_20606() {
    let (_pool, app, fx) = bootstrap().await;

    let (status, env) =
        login_admin(app, &fx.lonely_username, IamFixture::PASSWORD).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(env["code"], 20606);
}

// ===========================================================================
// 2. me (2)
// ===========================================================================

#[tokio::test]
async fn me_success_returns_full_user_view() {
    let (pool, app, fx) = bootstrap().await;
    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = fresh_app(&pool).await;
    let (status, env) = send(app2, json_request("GET", "/iam/me", None, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["username"], fx.manager_username);
    assert_eq!(env["data"]["roles"][0], "MANAGER");
    assert_eq!(
        env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap(),
        fx.manager_user_id
    );
}

#[tokio::test]
async fn me_without_authorization_returns_401() {
    let (_pool, app, _fx) = bootstrap().await;

    let (status, env) = send(app, json_request("GET", "/iam/me", None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(env["code"], 0);
}

// ===========================================================================
// 3. refresh (2)
// ===========================================================================

#[tokio::test]
async fn refresh_rotates_token_and_bumps_version() {
    let (pool, app, fx) = bootstrap().await;

    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let refresh_token = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // refresh 前 version 应为 0
    let ver_before: i32 = sqlx::query_scalar!(
        "SELECT refresh_token_version AS \"ver!\" FROM t_user WHERE id = $1",
        fx.manager_user_id
    )
    .fetch_one(&pool)
    .await
    .expect("query refresh_token_version");
    assert_eq!(ver_before, 0, "新建用户 refresh_token_version=0");

    let app2 = fresh_app(&pool).await;
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

    // refresh 后 version 应当 +1
    let ver_after: i32 = sqlx::query_scalar!(
        "SELECT refresh_token_version AS \"ver!\" FROM t_user WHERE id = $1",
        fx.manager_user_id
    )
    .fetch_one(&pool)
    .await
    .expect("query refresh_token_version");
    assert_eq!(ver_after, 1, "refresh 后 version 应当 +1");
}

#[tokio::test]
async fn refresh_reusing_old_token_returns_40105() {
    // 2026-09-23 重构：reuse detection 接管"旧 refresh 二次使用"语义——
    // 第一次 refresh 时 `complete_refresh` 把旧 refresh jti 写黑名单（TTL 至 refresh_exp），
    // 第二次同 refresh 再调时 phase 1 `is_jti_revoked` 命中，40105 + force_logout，
    // 而不是 DB version check 的 40103。底层 DB 40103 路径仍然存在但被闸位抢答，
    // 详见 `refresh_reuse_detection_triggers_force_logout_and_40105` 端到端覆盖。
    let (pool, app, fx) = bootstrap().await;

    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let old_refresh = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // 第一次 refresh：成功
    let app2 = fresh_app(&pool).await;
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
    let app3 = fresh_app(&pool).await;
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
    // 必须在 bootstrap() 而非 setup() 下跑——`is_jti_revoked` 走 Redis 黑名单；
    // `bootstrap` 调 `test_state`（内部建 redis_pool），保证 session:* 与
    // revoked:* 是空状态（test_pool 派生 fresh DB + Redis session 自动空）。
    let (pool, app, fx) = bootstrap().await;

    // 1) login → J1 (old_refresh)
    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let j1 = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // 2) refresh(J1) → J2_access / J2_refresh
    let app2 = fresh_app(&pool).await;
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
    let app3 = fresh_app(&pool).await;
    let (s2, me_env) = send(app3, json_request("GET", "/iam/me", None, Some(&j2_access))).await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "新签发的 J2_access 必须立即可用（佐证 complete_refresh 已写 J2 session）: {me_env}"
    );

    // 4) refresh(J1) 再来一次 → 40105（reuse detection）
    let app4 = fresh_app(&pool).await;
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
    let app5 = fresh_app(&pool).await;
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
    let (pool, app, fx) = bootstrap().await;

    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let access_token = login_env["data"]["token"].as_str().unwrap().to_string();
    let old_refresh = login_env["data"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // 改密
    let app2 = fresh_app(&pool).await;
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
    let app3 = fresh_app(&pool).await;
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
    let (pool, app, fx) = bootstrap().await;

    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = fresh_app(&pool).await;
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
    let (pool, app, fx) = bootstrap().await;
    let (_, login_env) =
        login_admin(app, &fx.clerk_username, IamFixture::PASSWORD).await;
    let clerk_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = fresh_app(&pool).await;
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
    let (pool, app, fx) = bootstrap().await;
    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let admin_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let app2 = fresh_app(&pool).await;
    let uri = format!("/iam/users/{}/roles", fx.target_user_id);
    let (status, env) = send(
        app2,
        json_request(
            "POST",
            &uri,
            Some(json!({
                "role": "SHELF_ACCOUNT",
                "scope_type": "shelf",
                "scope_id": fx.shelf_a_id,
            })),
            Some(&admin_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "add_role: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["role"], "SHELF_ACCOUNT");
    assert_eq!(env["data"]["shelf_code"], "FX-SH-A1");
}

#[tokio::test]
async fn add_duplicate_role_returns_409() {
    let (pool, app, fx) = bootstrap().await;
    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let admin_token = login_env["data"]["token"].as_str().unwrap().to_string();

    let uri = format!("/iam/users/{}/roles", fx.target_user_id);
    let body = json!({
        "role": "SHELF_ACCOUNT",
        "scope_type": "shelf",
        "scope_id": fx.shelf_b_id,
    });

    // 第一次：成功
    let app2 = fresh_app(&pool).await;
    let (s1, _) = send(
        app2,
        json_request("POST", &uri, Some(body.clone()), Some(&admin_token)),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    // 第二次：重复 → 409
    let app3 = fresh_app(&pool).await;
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

#[tokio::test]
async fn logout_kills_current_session() {
    let (pool, app, fx) = bootstrap().await;

    // 1) login → token A
    let (_, login_env) =
        login_admin(app, &fx.manager_username, IamFixture::PASSWORD).await;
    let token_a = login_env["data"]["token"].as_str().unwrap().to_string();

    // 2) /iam/me(A) 200
    let app2 = fresh_app(&pool).await;
    let (s, _) = send(app2, json_request("GET", "/iam/me", None, Some(&token_a))).await;
    assert_eq!(s, StatusCode::OK, "login 后 /iam/me 必须 200");

    // 3) logout(A) 200
    let app3 = fresh_app(&pool).await;
    let (lo_status, lo_env) = send(
        app3,
        json_request("POST", "/iam/logout", None, Some(&token_a)),
    )
    .await;
    assert_eq!(lo_status, StatusCode::OK, "logout 200: {lo_env}");

    // 4) /iam/me(A) → 40105（SESSION_REVOKED）
    let app4 = fresh_app(&pool).await;
    let (status, env) = send(app4, json_request("GET", "/iam/me", None, Some(&token_a))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "logout 后 /iam/me 必须 401: {env}"
    );
    assert_eq!(env["code"], 40105, "必须是 SESSION_REVOKED: {env}");
}