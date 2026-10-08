//! 企业微信小程序登录（`POST /api/v2/wx/iam/wx-login`）集成测试（2026-09-29 新增）
//!
//! 覆盖 15 条 HTTP 契约：
//!  1. 成功 → 200 + `data.token` 非空 + `data.user.roles` 正确
//!  2. 企微返 40029 → 40106
//!  3. userid 未预绑定 → 40107
//!  4. corpid 与配置不符 → 40107
//!  5. 绑定指向已停用用户 → 40101
//!  6. 绑定用户无角色 → 20606
//!  7. 后端未配置企业微信（NoopWeComClient）→ 40109
//!  8. 无 Authorization 头不被 401 拦截（验 auth 侧白名单真的生效）
//!  9. 带 `Idempotency-Key` 重复请求不返回缓存的同一份 token（验 idem 侧白名单）
//!     10-14. wx-bind 管理端点（5 条，见文件下半部分）
//! 15. wx-login 签发的 token 能正常访问 `/api/v2/iam/me`（端到端证 Redis session 写对了）
//!
//! ## 测试栈
//! - `MockWeComApiClient`（mockall `#[automock]`，主 lib 常驻导出）替换
//!   `state.wecom`，模拟企微 `jscode2session` 的各种返回 / 失败。
//! - `test_state_with_wecom(pool, client, corpid)` 注入 mock + 设定 `WECOM_CORPID`。
//! - fixture：`load_iam_fixture`（5 用户 + 2 角色）+ `load_wecom_fixture`（5 条
//!   `t_wx_identity` 预绑定），常量 ID 区段 110-118 / 120-124。
//!
//! ## 不用 test_app / v2_router 的历史坑
//! `tests/files_sts_tmp_keys.rs` 头部记录过「`v2_router()` 因 wx 模块路由重叠
//! panic」的历史 bug。2026-09-29 实测 `v2_router()` 已可正常构造（该 bug 已修），
//! 故本文件直接用 `test_support::test_app` 走生产同形路由——白名单两条断言
//! （第 8 / 9 条）**必须**在真实 `v2_router` 上跑才有意义。
//!
//! ## 并行注意
//! `test_pool()` 每次 fresh database，DB 间 schema 完全独立（`t_wx_identity`
//! 的 partial unique 索引不会跨测试撞车）。Redis 侧 wx-login 只写
//! `session:tok:*`（2026-10-09 起带 `t{pid}:` 进程前缀），`test_state` 连的是
//! `redis-test` 实例、与 dev 的 `redis-dev` 实例隔离。

use std::sync::Arc;

use axum::http::{StatusCode, header::AUTHORIZATION, header::HeaderName};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::modules::wx::wecom_client::{
    MockWeComApiClient, NoopWeComClient, WeComApiClient, WeComSession,
};
use hsh_erp_rust::shared::error::{AppError, code};
use hsh_erp_test_support::{
    IamFixture, WecomFixture, json_request, load_iam_fixture, load_wecom_fixture, login_token,
    send, test_app, test_pool,
};

/// wx-login 端点路径（测试直接挂 `v2_router`，无 `/api/v2` 前缀）。
const WX_LOGIN: &str = "/wx/iam/wx-login";

// ===========================================================================
// Helpers
// ===========================================================================

/// 构造一个「成功换身份」的 mock：返回 `(corpid, userid)`。
fn mock_ok(corp_id: &str, userid: &str) -> MockWeComApiClient {
    let corp_id = corp_id.to_string();
    let userid = userid.to_string();
    let mut m = MockWeComApiClient::default();
    m.expect_code_to_session().returning(move |_code| {
        Ok(WeComSession {
            corp_id: corp_id.clone(),
            user_id: userid.clone(),
        })
    });
    m
}

/// 构造一个「企微返回指定 errcode」的 mock（模拟 40029 等失败）。
fn mock_err(biz_code: i32) -> MockWeComApiClient {
    let mut m = MockWeComApiClient::default();
    m.expect_code_to_session()
        .returning(move |_code| Err(AppError::biz(biz_code, "mock wecom failure")));
    m
}

/// 基础 bootstrap：fresh DB + iam fixture + wecom fixture + mock 注入 + app。
async fn bootstrap_with_wecom(
    wecom: Arc<dyn WeComApiClient>,
    corp_id: &str,
) -> (PgPool, axum::Router, IamFixture, WecomFixture) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let wfx = load_wecom_fixture(&pool).await;
    let state = hsh_erp_test_support::test_state_with_wecom(pool.clone(), wecom, corp_id).await;
    let app = test_app(state);
    (pool, app, fx, wfx)
}

/// 成功路径的 bootstrap（mock 返回 fixture 的 MANAGER 绑定）
async fn bootstrap() -> (PgPool, axum::Router, IamFixture, WecomFixture) {
    bootstrap_with_wecom(
        Arc::new(mock_ok(
            WecomFixture::CORP_ID,
            WecomFixture::MANAGER_WX_USER_ID,
        )),
        WecomFixture::CORP_ID,
    )
    .await
}

/// 发一次 wx-login（不带 Authorization 头——本端点是公开路径）
async fn wx_login(app: axum::Router, code: &str) -> (StatusCode, Value) {
    send(
        app,
        json_request("POST", WX_LOGIN, Some(json!({ "code": code })), None),
    )
    .await
}

/// 每个请求重新构造 Router（axum 0.8 `oneshot` 消耗 app）
async fn fresh_app(pool: &PgPool) -> axum::Router {
    test_app(
        hsh_erp_test_support::test_state_with_wecom(
            pool.clone(),
            Arc::new(mock_ok(
                WecomFixture::CORP_ID,
                WecomFixture::MANAGER_WX_USER_ID,
            )),
            WecomFixture::CORP_ID,
        )
        .await,
    )
}

// ===========================================================================
// 1. wx-login 成功（200 + token + roles）
// ===========================================================================

#[tokio::test]
async fn wx_login_success_returns_token_pair_with_correct_roles() {
    let (pool, app, fx, _wfx) = bootstrap().await;

    let (status, env) = wx_login(app, "valid-code").await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(env["code"], 0);

    let data = &env["data"];
    let token = data["token"].as_str().expect("data.token");
    assert!(token.len() > 20, "access token 不应为空：{token}");
    assert!(data["refresh_token"].as_str().expect("refresh").len() > 20);
    assert_eq!(data["user"]["username"], fx.manager_username);
    assert_eq!(data["user"]["roles"][0], "MANAGER");
    assert_eq!(
        data["user"]["id"].as_str().unwrap().parse::<i64>().unwrap(),
        fx.manager_user_id
    );

    // last_login_at 已在同一事务内被 stamp
    let row = sqlx::query!(
        "SELECT last_login_at AS \"last?\" FROM t_user WHERE id = $1",
        fx.manager_user_id
    )
    .fetch_one(&pool)
    .await
    .expect("query last_login_at");
    assert!(row.last.is_some(), "wx-login 应写入 last_login_at");
}

// ===========================================================================
// 2. 企微 40029 → 40106
// ===========================================================================

#[tokio::test]
async fn wx_login_wecom_40029_returns_40106() {
    let (pool, _app, _fx, _wfx) = bootstrap_with_wecom(
        Arc::new(mock_err(code::BIZ_WX_LOGIN_FAILED)),
        WecomFixture::CORP_ID,
    )
    .await;
    let app = fresh_app_err(&pool, code::BIZ_WX_LOGIN_FAILED).await;

    let (status, env) = wx_login(app, "bad-code").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "envelope = {env}");
    assert_eq!(env["code"], 40106);
}

/// 用「指定错误码」的 mock 重建 app
async fn fresh_app_err(pool: &PgPool, biz_code: i32) -> axum::Router {
    test_app(
        hsh_erp_test_support::test_state_with_wecom(
            pool.clone(),
            Arc::new(mock_err(biz_code)),
            WecomFixture::CORP_ID,
        )
        .await,
    )
}

// ===========================================================================
// 3. userid 未预绑定 → 40107
// ===========================================================================

#[tokio::test]
async fn wx_login_unbound_userid_returns_40107() {
    let (_pool, _app, _fx, _wfx) = bootstrap_with_wecom(
        Arc::new(mock_ok(
            WecomFixture::CORP_ID,
            WecomFixture::UNBOUND_WX_USER_ID,
        )),
        WecomFixture::CORP_ID,
    )
    .await;
    let app = fresh_app_with_userid(WecomFixture::UNBOUND_WX_USER_ID).await;

    let (status, env) = wx_login(app, "valid-code").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "envelope = {env}");
    assert_eq!(env["code"], 40107);
    assert!(
        env["message"].as_str().unwrap().contains("未绑定"),
        "错误文案应指向绑定问题：{}",
        env["message"]
    );
}

// ===========================================================================
// 4. corpid 与配置不符 → 40107
// ===========================================================================

#[tokio::test]
async fn wx_login_corp_id_mismatch_returns_40107() {
    // 后端配置 CORP_ID，但企微返回 OTHER_CORP_ID → 防跨企业串号闸拦下
    let app = fresh_app_with_userid_from_corp(
        WecomFixture::CORP_ID,
        WecomFixture::OTHER_CORP_ID,
        WecomFixture::MANAGER_WX_USER_ID,
    )
    .await;

    let (status, env) = wx_login(app, "valid-code").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "envelope = {env}");
    assert_eq!(env["code"], 40107);
}

/// 用指定 userid 重建 app（corpid 固定为 fixture）
async fn fresh_app_with_userid(userid: &str) -> axum::Router {
    fresh_app_with_userid_from_corp(WecomFixture::CORP_ID, WecomFixture::CORP_ID, userid).await
}

/// 用「配置的 corpid / mock 返回的 corpid / mock 返回的 userid」重建 app
async fn fresh_app_with_userid_from_corp(
    config_corp: &str,
    mock_corp: &str,
    userid: &str,
) -> axum::Router {
    let pool = test_pool().await;
    let _fx = load_iam_fixture(&pool).await;
    let _wfx = load_wecom_fixture(&pool).await;
    let state = hsh_erp_test_support::test_state_with_wecom(
        pool,
        Arc::new(mock_ok(mock_corp, userid)),
        config_corp,
    )
    .await;
    test_app(state)
}

// ===========================================================================
// 5. 绑定指向已停用用户 → 40101
// ===========================================================================

#[tokio::test]
async fn wx_login_inactive_bound_user_returns_40101() {
    let app = fresh_app_with_userid(WecomFixture::INACTIVE_WX_USER_ID).await;

    let (status, env) = wx_login(app, "valid-code").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "envelope = {env}");
    assert_eq!(env["code"], 40101);
}

// ===========================================================================
// 6. 绑定用户无角色 → 20606
// ===========================================================================

#[tokio::test]
async fn wx_login_bound_user_without_roles_returns_20606() {
    let app = fresh_app_with_userid(WecomFixture::LONELY_WX_USER_ID).await;

    let (status, env) = wx_login(app, "valid-code").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "envelope = {env}");
    assert_eq!(env["code"], 20606);
}

// ===========================================================================
// 7. 未配置企业微信（NoopWeComClient）→ 40109
// ===========================================================================

#[tokio::test]
async fn wx_login_not_configured_returns_40109() {
    // corp_id 传空串 → test_state_with_wecom 装 enabled=false 的 Noop 语义
    // （本测试显式注入 NoopWeComClient，与 main.rs 的降级路径同形）
    let pool = test_pool().await;
    let _fx = load_iam_fixture(&pool).await;
    let _wfx = load_wecom_fixture(&pool).await;
    let state =
        hsh_erp_test_support::test_state_with_wecom(pool, Arc::new(NoopWeComClient), "").await;
    let app = test_app(state);

    let (status, env) = wx_login(app, "valid-code").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "envelope = {env}");
    assert_eq!(env["code"], 40109);
}

// ===========================================================================
// 8. 无 Authorization 头不被 401 拦截（auth 侧白名单）
// ===========================================================================

#[tokio::test]
async fn wx_login_without_authorization_header_is_not_401() {
    let (_pool, _app, _fx, _wfx) = bootstrap().await;
    let app = fresh_app(&_pool).await;

    // 请求体里**没有** Authorization 头（`wx_login` helper 不加）
    let req = json_request(
        "POST",
        WX_LOGIN,
        Some(json!({ "code": "valid-code" })),
        None,
    );
    assert!(
        req.headers().get(AUTHORIZATION).is_none(),
        "本用例前提：请求不带 Authorization 头"
    );
    let (status, env) = send(app, req).await;

    // 若白名单缺失，这里会是 40100；命中白名单则走到 handler 正常业务结果
    assert_ne!(env["code"], 40100, "白名单失效：{env}");
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(env["code"], 0);
}

// ===========================================================================
// 9. Idempotency-Key 重复请求不返回缓存的同一份 token（idem 侧白名单）
// ===========================================================================

#[tokio::test]
async fn wx_login_idempotency_key_does_not_cache_jwt_response() {
    let pool = test_pool().await;
    let _fx = load_iam_fixture(&pool).await;
    let _wfx = load_wecom_fixture(&pool).await;

    // 同一个 key 连打两次：若 idem 白名单缺失，第 2 次会命中缓存返回
    // **第 1 次的 token**（= 跨用户 session 劫持漏洞）。
    let key = "wx-login-fixed-key";
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let state = hsh_erp_test_support::test_state_with_wecom(
            pool.clone(),
            Arc::new(mock_ok(
                WecomFixture::CORP_ID,
                WecomFixture::MANAGER_WX_USER_ID,
            )),
            WecomFixture::CORP_ID,
        )
        .await;
        let app = test_app(state);
        let mut req = json_request(
            "POST",
            WX_LOGIN,
            Some(json!({ "code": "valid-code" })),
            None,
        );
        req.headers_mut().insert(
            HeaderName::from_static("idempotency-key"),
            key.parse().unwrap(),
        );
        let (status, env) = send(app, req).await;
        assert_eq!(status, StatusCode::OK, "envelope = {env}");
        tokens.push(env["data"]["token"].as_str().unwrap().to_string());
    }

    assert_eq!(tokens.len(), 2);
    assert_ne!(
        tokens[0], tokens[1],
        "两次 wx-login 返回了同一份 token —— idempotency 白名单失效（session 劫持漏洞）"
    );
}

// ===========================================================================
// 10-14. wx-bind 管理端点
// ===========================================================================

/// wx-bind 相关 bootstrap：注入「mock 换到 clerk 身份」的 app + 额外提供
/// 一路 token 绑定的 state。
///
/// 返回 `(app 工厂, manager token, clerk token, fx, wfx)`。
/// 每次请求都需新 app，故返回 `pool` 由各用例现场 `bind_app` 重建。
async fn bind_bootstrap() -> (PgPool, IamFixture, WecomFixture, String, String) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let wfx = load_wecom_fixture(&pool).await;
    let state = hsh_erp_test_support::test_state_with_wecom(
        pool.clone(),
        Arc::new(mock_ok(
            WecomFixture::CORP_ID,
            WecomFixture::CLERK_WX_USER_ID,
        )),
        WecomFixture::CORP_ID,
    )
    .await;
    let app = test_app(state);
    let manager_token = login_token(&app, &fx.manager_username, IamFixture::PASSWORD).await;
    let clerk_token = login_token(&app, &fx.clerk_username, IamFixture::PASSWORD).await;
    (pool, fx, wfx, manager_token, clerk_token)
}

/// wx-bind 场景的 app 工厂（mock 与 corpid 同 bind_bootstrap）
async fn bind_app(pool: &PgPool) -> axum::Router {
    test_app(
        hsh_erp_test_support::test_state_with_wecom(
            pool.clone(),
            Arc::new(mock_ok(
                WecomFixture::CORP_ID,
                WecomFixture::CLERK_WX_USER_ID,
            )),
            WecomFixture::CORP_ID,
        )
        .await,
    )
}

#[tokio::test]
async fn wx_bind_manager_can_bind_unbound_userid() {
    let (pool, _fx, _wfx, mgr, _clerk) = bind_bootstrap().await;

    // wecom fixture 给 5 个账号都建了绑定行（system→wx 方向已占），故先建一个
    // 全新账号来演示「绑一个没人用的 userid」
    let (status, env) = send(
        bind_app(&pool).await,
        json_request(
            "POST",
            "/iam/users",
            Some(json!({
                "username": "wxbindnew",
                "password": "pwd-12345",
                "full_name": "Wx Bind New",
            })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "建新账号: {env}");
    let new_user_id = env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();

    let (status, env) = send(
        bind_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{new_user_id}/wx-bind"),
            Some(json!({ "wx_user_id": "  NewWxUser  " })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(env["code"], 0);
    // userid 已归一化：trim + 转小写
    assert_eq!(env["data"]["wx_user_id"], "newwxuser");
    // corp_id 省略 → 取后端 WECOM_CORPID
    assert_eq!(env["data"]["corp_id"], WecomFixture::CORP_ID);
}

#[tokio::test]
async fn wx_bind_clerk_role_is_forbidden() {
    let (pool, fx, _wfx, _mgr, clerk) = bind_bootstrap().await;

    let (status, env) = send(
        bind_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": "clerk-attempt" })),
            Some(&clerk),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "envelope = {env}");
    assert_eq!(env["code"], 40300);
}

#[tokio::test]
async fn wx_bind_same_userid_to_other_user_returns_40108() {
    let (pool, fx, _wfx, mgr, _clerk) = bind_bootstrap().await;

    // fx_wx_target 已绑到 fx_iam_target（fixture 124 行）；改绑到 manager → 40108
    let (status, env) = send(
        bind_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.manager_user_id),
            Some(json!({ "wx_user_id": WecomFixture::TARGET_WX_USER_ID })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "envelope = {env}");
    assert_eq!(env["code"], 40108);
}

#[tokio::test]
async fn wx_bind_same_account_rebind_is_idempotent_200() {
    let (pool, fx, _wfx, mgr, _clerk) = bind_bootstrap().await;

    // fixture 里 fx_wx_target → fx_iam_target 已存在；重复绑同一 user 幂等成功
    let (status, env) = send(
        bind_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": WecomFixture::TARGET_WX_USER_ID })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["wx_user_id"], WecomFixture::TARGET_WX_USER_ID);
    // 幂等语义：返回的是既有绑定行 id（不是新建行）
    assert_eq!(
        env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap(),
        WecomFixture::TARGET_BIND_ID
    );
}

#[tokio::test]
async fn wx_unbind_then_rebind_succeeds() {
    let (pool, fx, _wfx, mgr, _clerk) = bind_bootstrap().await;
    let url = format!("/iam/users/{}/wx-bind", fx.target_user_id);
    let unbind_url = format!("{url}/unbind");
    let version: i32 = sqlx::query_scalar!(
        "SELECT version AS \"v!\" FROM t_wx_identity WHERE id = $1",
        WecomFixture::TARGET_BIND_ID
    )
    .fetch_one(&pool)
    .await
    .expect("query bind version");

    // GET：解绑前能看到 fixture 绑定（业务上双向一对一 ⇒ data 是单对象）
    let (status, env) = send(
        bind_app(&pool).await,
        json_request("GET", &url, None, Some(&mgr)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(env["data"]["wx_user_id"], WecomFixture::TARGET_WX_USER_ID);

    // POST .../unbind：软删（body 带 OCC 锚点 version）
    let (status, env) = send(
        bind_app(&pool).await,
        json_request(
            "POST",
            &unbind_url,
            Some(json!({ "version": version })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert!(env["data"].is_null(), "unbind 应返 R<()>：{env}");

    // 幂等：再解绑一次仍成功（无绑定）
    let (status, env) = send(
        bind_app(&pool).await,
        json_request(
            "POST",
            &unbind_url,
            Some(json!({ "version": version })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");

    // 解绑后可重新绑定同一个 userid（partial unique 索引释放了坑位）
    let (status, env) = send(
        bind_app(&pool).await,
        json_request(
            "POST",
            &url,
            Some(json!({ "wx_user_id": WecomFixture::TARGET_WX_USER_ID })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    // 新建行（新 id ≠ 被软删的 fixture 行）
    assert_ne!(
        env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap(),
        WecomFixture::TARGET_BIND_ID
    );
}

// ===========================================================================
// 15. wx-login 签发的 token 能正常访问 /iam/me（端到端证 Redis session）
// ===========================================================================

#[tokio::test]
async fn wx_login_issued_token_works_on_iam_me() {
    let (pool, _app, fx, _wfx) = bootstrap().await;

    // 1) wx-login 拿 token
    let (status, env) = wx_login(fresh_app(&pool).await, "valid-code").await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    let token = env["data"]["token"].as_str().unwrap().to_string();

    // 2) 用该 token 访问 /iam/me。
    //    ⚠️ 这是最容易漏的回归点：若 handler 只签 JWT 不调 complete_login
    //    写 Redis session，这里会拿到 40105 SESSION_REVOKED。
    let (status, env) = send(
        fresh_app(&pool).await,
        json_request("GET", "/iam/me", None, Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["username"], fx.manager_username);
    assert_eq!(env["data"]["roles"][0], "MANAGER");
}
