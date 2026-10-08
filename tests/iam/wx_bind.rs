//! iam 域企业微信绑定端点补充测试
//!
//! 与 `tests/wecom_login.rs` 的分工：那里覆盖 wx-login 主链路 + 白名单；
//! 本文件聚焦 `/api/v2/iam/users/{id}/wx-bind*` 的 **service 层校验分支**
//! （入参归一化 / 配置回退 / 长度上限 / 双向一对一 / OCC 冲突），两者合起来覆盖
//! 这 3 个端点的全部可机械核对项。
//!
//! 端点形态（服务层校验分支见 `src/modules/iam/service/account/wx.rs`）：
//! - `POST /api/v2/iam/users/{id}/wx-bind` —— 绑定（幂等；userid 已属他人 → 40108；
//!   本账号已绑别的 userid → 40110）
//! - `GET  /api/v2/iam/users/{id}/wx-bind` —— 查当前绑定（未绑定 → `data: null`）
//! - `POST /api/v2/iam/users/{id}/wx-bind/unbind` —— 解绑（body 带 `version`；
//!   幂等；无绑定 → 200）
//!
//! 2026-10-10 破坏性变更：`DELETE /api/v2/iam/users/{id}/wx-bind` **已删、无 alias**
//! （`wx_unbind_old_delete_route_is_gone` 一条钉住 405）。

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_rust::modules::wx::wecom_client::NoopWeComClient;
use hsh_erp_test_support::{
    IamFixture, WecomFixture, json_request, load_iam_fixture, load_wecom_fixture, login_token,
    send, send_raw, test_app, test_pool,
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
/// 绑定端点只读 `state.config.wecom.corpid`，**不碰**企微接口，所以 wecom 客户端塞
/// `NoopWeComClient` 即可（只为换 config）。
async fn fresh_app_with_corp(pool: &PgPool, corp_id: &str) -> axum::Router {
    let state = hsh_erp_test_support::test_state_with_wecom(
        pool.clone(),
        Arc::new(NoopWeComClient),
        corp_id,
    )
    .await;
    test_app(state)
}

/// 读一条绑定行的当前 `version`（解绑的 OCC 锚点）。
async fn bind_version(pool: &PgPool, bind_id: i64) -> i32 {
    sqlx::query_scalar!(
        "SELECT version AS \"v!\" FROM t_wx_identity WHERE id = $1",
        bind_id
    )
    .fetch_one(pool)
    .await
    .expect("query wx_identity.version")
}

// ===========================================================================
// 1. GET 读端点
// ===========================================================================

#[tokio::test]
async fn wx_bind_get_returns_single_object_when_bound() {
    let (pool, _app, fx, wfx, mgr) = bootstrap().await;

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "GET",
            &format!("/iam/users/{}/wx-bind", fx.manager_user_id),
            None,
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    // 业务上双向一对一 ⇒ data 是**单对象**，不是数组
    let item = &env["data"];
    assert!(item.is_object(), "data 应是单个对象：{env}");
    assert_eq!(item["wx_user_id"], WecomFixture::MANAGER_WX_USER_ID);
    assert_eq!(item["corp_id"], WecomFixture::CORP_ID);
    assert_eq!(
        item["id"].as_str().unwrap().parse::<i64>().unwrap(),
        wfx.manager_bind_id
    );
    // version 是 OCC 锚点，前端解绑时要用
    assert_eq!(
        item["version"].as_i64(),
        Some(bind_version(&pool, wfx.manager_bind_id).await as i64)
    );
}

#[tokio::test]
async fn wx_bind_get_without_binding_returns_null() {
    let (pool, _app, _fx, _wfx, mgr) = bootstrap().await;

    // 用一个不存在的账号 ID（999 号段）验证「无绑定 → data: null」而非 404：
    // 本端点不做目标账号存在性校验（它只是查绑定，语义上等价于「查不到」）
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
    assert_eq!(env["code"], 0);
    assert!(
        env["data"].is_null(),
        "无绑定账号应返回 data: null（不是空数组）：{env}"
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

/// 请求体带 `corp_id` 仍是**未知字段**（DTO 已删该字段），serde 默认忽略未知字段
/// 不报错，落库一律用后端配置的 `WECOM_CORPID`。这条钉住「删字段」不破坏旧客户端。
#[tokio::test]
async fn wx_bind_ignores_unknown_corp_id_field_and_uses_configured_one() {
    let (pool, _app, _fx, _wfx, mgr) = bootstrap().await;

    // wecom fixture 给 5 个账号都建了绑定行（system→wx 已占），故先建一个全新账号
    let (status, env) = send(
        fresh_app_with_corp(&pool, WecomFixture::CORP_ID).await,
        json_request(
            "POST",
            "/iam/users",
            Some(json!({
                "username": "brandnew",
                "password": "pwd-12345",
                "full_name": "Brand New",
            })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "建新账号: {env}");
    let new_user_id = env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();

    // 传一个与配置不同的 corp_id + 混合大小写 userid
    // → 落库为配置的 corp + 小写 userid
    let (status, env) = send(
        fresh_app_with_corp(&pool, WecomFixture::CORP_ID).await,
        json_request(
            "POST",
            &format!("/iam/users/{new_user_id}/wx-bind"),
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

/// 请求体带 corp_id 也**不能**绕过「后端未配置 WECOM_CORPID → 40109」这道闸
/// （否则就是一条「配了但生效不了」的隐性配置路径）。
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
// 3. 绑定基数：双向一对一
// ===========================================================================

/// wx → system 方向：该 userid 已属别的账号 → 40108 / 409
#[tokio::test]
async fn wx_bind_rejects_userid_owned_by_another_user() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;

    // lonely_user 在 wecom fixture 里有 `fx_wx_lonely` 的绑定；这里拿 manager 已占的
    // `fx_wx_manager` 去绑 lonely_user ⇒ wx→system 冲突
    let (status, env) = send(
        fresh_app_with_corp(&pool, WecomFixture::CORP_ID).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.lonely_user_id),
            Some(json!({ "wx_user_id": WecomFixture::MANAGER_WX_USER_ID })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "envelope = {env}");
    assert_eq!(env["code"], 40108, "envelope = {env}");
}

/// wx → system 方向：同账号同 userid 重复绑 → 幂等 200，且不新增行
#[tokio::test]
async fn wx_bind_same_userid_is_idempotent() {
    let (pool, _app, fx, wfx, mgr) = bootstrap().await;
    let before = bind_version(&pool, wfx.target_bind_id).await;

    let (status, env) = send(
        fresh_app_with_corp(&pool, WecomFixture::CORP_ID).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": "fx_wx_target" })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(
        env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap(),
        wfx.target_bind_id,
        "幂等应返回已有行"
    );

    let n: i64 = sqlx::query_scalar!(
        "SELECT count(*) AS \"n!\" FROM t_wx_identity WHERE user_id = $1",
        fx.target_user_id
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(n, 1, "幂等重复绑不应新增行");
    assert_eq!(
        bind_version(&pool, wfx.target_bind_id).await,
        before,
        "幂等重复绑不应改动已有行"
    );
}

/// system → wx 方向：本账号已绑别的 userid → 40110 / 409
#[tokio::test]
async fn wx_bind_rejects_second_distinct_userid_on_same_account() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;

    let (status, env) = send(
        fresh_app_with_corp(&pool, WecomFixture::CORP_ID).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind", fx.target_user_id),
            Some(json!({ "wx_user_id": "brand-new-userid" })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "envelope = {env}");
    assert_eq!(
        env["code"], 40110,
        "一个系统账号只能绑一个企业微信 userid：{env}"
    );
}

// ===========================================================================
// 4. POST 解绑
// ===========================================================================

#[tokio::test]
async fn wx_unbind_soft_deletes_and_is_idempotent() {
    let (pool, _app, fx, wfx, mgr) = bootstrap().await;
    let url = format!("/iam/users/{}/wx-bind/unbind", fx.manager_user_id);
    let version = bind_version(&pool, wfx.manager_bind_id).await;

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &url,
            Some(json!({ "version": version })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert_eq!(env["code"], 0);
    // 2026-10-10：返回值收敛为 R<()>（data: null），不再回逐行快照
    assert!(env["data"].is_null(), "unbind 应返 R<()>：{env}");

    // DB 侧确认为软删（deleted_at 非空），不是物理删除；version 推进 +1
    let row = sqlx::query!(
        "SELECT deleted_at AS \"d?\", version FROM t_wx_identity WHERE id = $1",
        wfx.manager_bind_id
    )
    .fetch_one(&pool)
    .await
    .expect("query deleted_at");
    assert!(row.d.is_some(), "解绑必须是软删（deleted_at 非空）");
    assert_eq!(
        row.version,
        version + 1,
        "soft_delete 的 SQL 是 version = version + 1"
    );

    // GET 现在应返 data: null
    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "GET",
            &format!("/iam/users/{}/wx-bind", fx.manager_user_id),
            None,
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
    assert!(env["data"].is_null(), "解绑后 GET 应返 null：{env}");

    // 幂等：再解绑一次 → 200
    let (status, env) = send(
        fresh_app(&pool).await,
        json_request("POST", &url, Some(json!({ "version": 0 })), Some(&mgr)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "envelope = {env}");
}

/// 缺 `version` → axum `Json` 提取器返 422 **纯文本**（不是业务信封）
#[tokio::test]
async fn wx_unbind_without_version_returns_422_plain_text() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;

    // ⚠️ 422 走 axum `Json` 提取器，body 是**纯文本**不是业务信封 ⇒ 必须用
    // `send_raw`（`send` 会在 JSON 解析处 panic）
    let (status, body) = send_raw(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind/unbind", fx.manager_user_id),
            Some(json!({})),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body = {body}");
    assert!(
        !body.contains("\"code\""),
        "422 是纯文本不是业务信封，实际 body = {body}"
    );
}

/// version 与 DB 不符 → 409 / 40901
#[tokio::test]
async fn wx_unbind_version_mismatch_returns_409() {
    let (pool, _app, fx, wfx, mgr) = bootstrap().await;

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind/unbind", fx.manager_user_id),
            Some(json!({ "version": 999 })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "envelope = {env}");
    assert_eq!(env["code"], 40901, "envelope = {env}");

    // 冲突时整笔回滚：行仍在
    let row = sqlx::query!(
        "SELECT deleted_at AS \"d?\" FROM t_wx_identity WHERE id = $1",
        wfx.manager_bind_id
    )
    .fetch_one(&pool)
    .await
    .expect("query");
    assert!(row.d.is_none(), "OCC 冲突后不得真的软删");
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
    let version = bind_version(&pool, WecomFixture::MANAGER_BIND_ID).await;

    let (status, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/iam/users/{}/wx-bind/unbind", fx.manager_user_id),
            Some(json!({ "version": version })),
            Some(&clerk),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "envelope = {env}");
    assert_eq!(env["code"], 40300);
}

/// 旧路由 `DELETE /iam/users/{id}/wx-bind` 已下线、无 alias ⇒ 405
#[tokio::test]
async fn wx_unbind_old_delete_route_is_gone() {
    let (pool, _app, fx, _wfx, mgr) = bootstrap().await;

    let (status, body) = send_raw(
        fresh_app(&pool).await,
        json_request(
            "DELETE",
            &format!("/iam/users/{}/wx-bind", fx.manager_user_id),
            None,
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "DELETE 已下线（无 alias），只应 405：{body}"
    );
}
