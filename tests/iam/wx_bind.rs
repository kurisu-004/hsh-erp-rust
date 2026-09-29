//! iam 域企业微信绑定端点补充测试（2026-09-29 新增）
//!
//! 与 `tests/wecom_login.rs` 的分工：那里覆盖 wx-login 主链路 + 白名单；
//! 本文件聚焦 `/api/v2/iam/users/{id}/wx-bind` 的 **service 层校验分支**
//! （入参归一化 / corp_id 回退 / 长度上限 / 空数组读端点），两者合起来覆盖
//! A10 端点的全部可机械核对项。
//!
//! 端点形态（详见 `docs/api/iam.md`）：
//! - `POST   /api/v2/iam/users/{id}/wx-bind` —— 绑定（幂等；跨账号 → 40108）
//! - `GET    /api/v2/iam/users/{id}/wx-bind` —— 查该用户全部绑定
//! - `DELETE /api/v2/iam/users/{id}/wx-bind` —— 解绑（幂等；无绑定 → 空数组）

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::modules::wx::wecom_client::NoopWeComClient;
use hsh_erp_test_support::{
    IamFixture, WecomFixture, json_request, load_iam_fixture, load_wecom_fixture, login_token,
    send, test_app, test_pool,
};

/// 基础 bootstrap：fresh DB + iam/wecom fixture + state + app + MANAGER token
///
/// 这里走默认 `test_state`（`wecom = NoopWeComClient`）——本文件的端点不碰
/// 企微接口，只写 `t_wx_identity`。
async fn bootstrap() -> (PgPool, axum::Router, IamFixture, WecomFixture, String) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let wfx = load_wecom_fixture(&pool).await;
    let state = hsh_erp_test_support::test_state(pool.clone()).await;
    let app = test_app(state);
    let token = login_token(&app, &fx.manager_username, IamFixture::PASSWORD).await;
    (pool, app, fx, wfx, token)
}

async fn fresh_app(pool: &PgPool) -> axum::Router {
    test_app(hsh_erp_test_support::test_state(pool.clone()).await)
}

/// 构造一个 **配了 `WECOM_CORPID`** 的 app。
///
/// 2026-09-29（review 第 1 轮 Y3）：绑定端点只读 `state.config.wecom.corpid`，
/// **不碰**企微接口，所以 wecom 客户端塞 `NoopWeComClient` 即可（只为换 config）。
async fn fresh_app_with_corp(pool: &PgPool, corp_id: &str) -> axum::Router {
    let state = hsh_erp_test_support::test_state_with_wecom(
        pool.clone(),
        Arc::new(NoopWeComClient),
        corp_id,
    )
    .await;
    test_app(state)
}

// ===========================================================================
// 1. GET 读端点
// ===========================================================================

#[tokio::test]
async fn wx_bind_get_returns_all_active_bindings_of_user() {
    let (_pool, _app, fx, wfx, mgr) = bootstrap().await;

    let (status, env) = send(
        fresh_app(&_pool).await,
        json_request(
            "GET",
            &format!("/iam/users/{}/wx-bind", fx.manager_user_id),
            None,
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    let items = env["data"].as_array().expect("data 必须是数组");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["wx_user_id"], WecomFixture::MANAGER_WX_USER_ID);
    assert_eq!(items[0]["corp_id"], WecomFixture::CORP_ID);
    assert_eq!(
        items[0]["id"].as_str().unwrap().parse::<i64>().unwrap(),
        wfx.manager_bind_id
    );
}

#[tokio::test]
async fn wx_bind_get_on_user_without_binding_returns_empty_array() {
    let (pool, _app, _fx, _wfx, mgr) = bootstrap().await;

    // 用一个不存在的账号 ID（999 号段）验证「无绑定 → 空数组」而非 404：
    // 本端点不做目标账号存在性校验（它只是列绑定，语义上等价于「查空集合」）
    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "GET",
            "/iam/users/900000000000009999/wx-bind",
            None,
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert!(
        env["data"].as_array().unwrap().is_empty(),
        "无绑定账号应返回空数组：{env}"
    );
}

#[tokio::test]
async fn wx_bind_get_requires_manager_role() {
    let (pool, _app, _fx, _wfx, _mgr) = bootstrap().await;
    let clerk = login_token(
        &fresh_app(&pool).await,
        "fx_iam_clerk",
        IamFixture::PASSWORD,
    )
    .await;

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request("GET", "/iam/users/1/wx-bind", None, Some(&clerk)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "envelope = {env}");
    assert_eq!(env["code"], 40300);
}

#[tokio::test]
async fn wx_bind_requires_authorization_header() {
    let (pool, _app, fx, _wfx, _mgr) = bootstrap().await;

    // 本端点**不在**公开白名单（与 /wx/iam/wx-login 不同）——无 token 必须 401
    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "GET",
            &format!("/iam/users/{}/wx-bind", fx.manager_user_id),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "envelope = {env}");
    assert_eq!(env["code"], 40100);
}

// ===========================================================================
// 2. POST 绑定入参校验
// ===========================================================================

#[tokio::test]
async fn wx_bind_rejects_blank_userid() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": "   " })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "envelope = {env}");
    assert_eq!(env["code"], 40001);
}

#[tokio::test]
async fn wx_bind_rejects_oversized_userid() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;
    let long = "a".repeat(65); // 超过 varchar(64)

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": long })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "envelope = {env}");
    assert_eq!(env["code"], 40001);
}

#[tokio::test]
async fn wx_bind_rejects_unknown_target_user() {
    let (pool, _app, _fx, _wfx, mgr) = bootstrap().await;

    // app 必须配了 WECOM_CORPID，否则 service 的第一个分支就是 40109（校验顺序：
    // 入参归一 → corp_id 取配置 → 目标账号存在性），断言落不到 20601 上。
    let (status, env) = send(
        fresh_app_with_corp(&pool, WecomFixture::CORP_ID).await,
        json_request(
            "POST",
            "/iam/users/900000000000009999/wx-bind",
            Some(json!({ "wx_user_id": "ghost-user" })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "envelope = {env}");
    assert_eq!(env["code"], 20601);
}

/// 2026-09-29（review 第 1 轮 Y3）：请求体的 `corp_id` **被忽略**，一律以后端
/// `WECOM_CORPID` 落库。
///
/// 初版把「`corp_id: "ww-second-corp"` 绑定成功」固化成了断言，但登录侧只认
/// 配置值——这样绑出来的行用户永远登不进来（40107），还会在别的企业命名空间
/// 占住 `(corp_id, wx_user_id)` 的唯一坑位。
#[tokio::test]
async fn wx_bind_ignores_request_corp_id_and_uses_configured_one() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;

    // 传一个与配置不同的 corp_id + 混合大小写 userid
    // → 落库为配置的 corp + 小写 userid
    let (status, env) = send(
        fresh_app_with_corp(&pool, WecomFixture::CORP_ID).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": "  MiXeDCase  ", "corp_id": "ww-second-corp" })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(
        env["data"]["wx_user_id"], "mixedcase",
        "userid 应归一化为小写"
    );
    assert_eq!(
        env["data"]["corp_id"],
        WecomFixture::CORP_ID,
        "corp_id 必须取后端配置，请求体的 ww-second-corp 应被忽略"
    );
}

#[tokio::test]
async fn wx_bind_returns_40109_when_backend_corp_id_blank() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;
    // state.config.wecom.corpid 为空串（test_state 默认 WeComConfig::default()）
    // → 无法确定企业 → 40109
    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": "no-corp-user" })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "envelope = {env}");
    assert_eq!(env["code"], 40109);
}

/// 2026-09-29（review 第 1 轮 Y3）：请求体带 corp_id 也**不能**绕过
/// 「后端未配置 WECOM_CORPID → 40109」这道闸（否则就是一条「配了但生效不了」
/// 的隐性配置路径）。
#[tokio::test]
async fn wx_bind_request_corp_id_cannot_bypass_40109_when_config_blank() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;
    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": "sneaky-corp-user", "corp_id": "ww-second-corp" })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "envelope = {env}");
    assert_eq!(env["code"], 40109);
}

// ===========================================================================
// 3. DELETE 解绑
// ===========================================================================

#[tokio::test]
async fn wx_unbind_soft_deletes_and_is_idempotent() {
    let (pool, _app, fx, wfx, mgr) = bootstrap().await;
    let url = format!("/iam/users/{}/wx-bind", fx.manager_user_id);

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request("DELETE", &url, None, Some(&mgr)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    let removed = env["data"].as_array().unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(
        removed[0]["id"].as_str().unwrap().parse::<i64>().unwrap(),
        wfx.manager_bind_id
    );

    // DB 侧确认为软删（deleted_at 非空），不是物理删除
    let row = sqlx::query!(
        "SELECT deleted_at AS \"d?\", version FROM t_wx_identity WHERE id = $1",
        wfx.manager_bind_id
    )
    .fetch_one(&pool)
    .await
    .expect("query deleted_at");
    assert!(row.d.is_some(), "解绑必须是软删（deleted_at 非空）");

    // 2026-09-29（review 第 1 轮 B3）：响应里的 version 必须是**软删之后**的值，
    // 即与 DB 现状一致；初版推的是删除**前**的旧值，会与 DB 差 1。
    assert_eq!(
        removed[0]["version"].as_i64(),
        Some(row.version as i64),
        "返回的 version 应等于软删后的 DB 值（初版返回的是删除前的旧值，会差 1）"
    );

    // 幂等：再删一次 → 200 + 空数组
    let (status, env) = send(
        fresh_app(&pool).await,
        json_request("DELETE", &url, None, Some(&mgr)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert!(env["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn wx_unbind_requires_manager_role() {
    let (pool, _app, fx, _wfx, _mgr) = bootstrap().await;
    let clerk = login_token(
        &fresh_app(&pool).await,
        "fx_iam_clerk",
        IamFixture::PASSWORD,
    )
    .await;

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "DELETE",
            &format!("/iam/users/{}/wx-bind", fx.manager_user_id),
            None,
            Some(&clerk),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "envelope = {env}");
    assert_eq!(env["code"], 40300);
}
