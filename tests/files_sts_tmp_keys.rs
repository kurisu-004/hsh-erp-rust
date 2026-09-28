//! files 域 BFF（`POST /api/v2/files/sts-tmp-keys`）集成测试（2026-09-28 新增）
//!
//! 覆盖：
//! 1. 未带 token → 40100 UNAUTHORIZED（middleware 自动）
//! 2. 带 MANAGER token + MockPyBackend 返回 200 + 信封 body → 200 + body 透传
//! 3. 带 MANAGER token + MockPyBackend 返回 502 → 502 + BIZ_STS_FORWARD_FAILED=20406
//!
//! ## 背景
//! 修复 python `POST /api/v1/files/sts-tmp-keys` 裸开鉴权漏洞——rust 端新增
//! 薄壳鉴权转发端点 `POST /api/v2/files/sts-tmp-keys`（详见
//! `/Users/ren/.claude/plans/sts-session-uploader-sts-sts-sequential-globe.md`）。
//!
//! ## 测试栈
//! - `tokio::test` + `tower::ServiceExt::oneshot` 直接调 Router
//! - `MockPyBackendClient`（mockall automock）替换 `state.py_backend` 模拟 python 后端响应
//!
//! ## 不走 test_app / v2_router 的原因
//! `v2_router` 当前因 wx 模块合并 bug（pre-existing）会在构造时 panic；
//! 本测试单独构造一个最小 Router（仅 `authenticate_middleware` + `/files` nest），
//! 避开 wx 模块的影响。鉴权与生产路径完全一致（同一 `authenticate_middleware`）。

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::py_backend::{
    MockPyBackendClient, PyBackendClient, PyBackendResponse,
};
use hsh_erp_rust::modules::files;
use hsh_erp_rust::shared::error::AppError;
use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token, send as ts_send, test_pool,
    test_state,
};

// ===========================================================================
// Helpers
// ===========================================================================

/// 把 request 发给 axum app，oneshot 出来，拆 (status, body JSON envelope)。
async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    ts_send(app, req).await
}

/// 构造最小 Router：仅含 `authenticate_middleware` + `/files` nest + iam login 路由。
///
/// ## 为什么不用 `test_app` / `v2_router`？
/// 2026-09-28 wx 模块合并有 pre-existing 路由重叠 bug（`parts::router()` 与
/// `batches::router()` 都注册 `GET /counts`），导致 `v2_router()` 在所有用
/// `test_app` 启的 integration test 里 panic。本测试独立构造最小 Router：
/// - 鉴权用 `auth::middleware::authenticate_middleware`（与生产路径同源）
/// - `/files` nest 由 `modules::files::router()` 提供
/// - `/iam/login` + `/iam/me` 用于 login_token helper + middleware 鉴权白名单
/// - 不挂 wx / dashboard / ws 等其它 nest，避免触发 wx panic
async fn build_minimal_app(state: Arc<hsh_erp_rust::state::AppState>) -> axum::Router {
    use axum::{Router, middleware};
    Router::new()
        .nest("/files", files::router())
        // 与 v2_router 的顺序对齐：authenticate_middleware 后调 = 外层 =
        // handler 之前最先跑（先鉴权）；idempotency 在本测试不需要，跳过。
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            hsh_erp_rust::auth::middleware::authenticate_middleware,
        ))
        // `POST /iam/login` 必须可达（test-support::login_token 内部走 /iam/login）。
        // 借助 iam::router() 完整 nest 即可（其内只暴露 login/me/logout/... 等，
        // 与本测试无关的端点不挂）。
        .nest("/iam", hsh_erp_rust::modules::iam::router())
        .with_state(state)
}

/// 基础 bootstrap：fresh DB + iam fixture 9 行 + state + minimal app + MANAGER token。
async fn bootstrap() -> (PgPool, axum::Router, IamFixture, String) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let app = build_minimal_app(state.clone()).await;
    let token = login_token(&app, &fx.manager_username, IamFixture::PASSWORD).await;
    (pool, app, fx, token)
}

/// 用 mockall MockPyBackendClient 替换 `state.py_backend` 后构造最小 Router。
async fn make_app_with_mock(
    pool: &PgPool,
    mock: MockPyBackendClient,
) -> (axum::Router, Arc<hsh_erp_rust::state::AppState>) {
    let py_backend: Arc<dyn PyBackendClient> = Arc::new(mock);
    let mut state = test_state(pool.clone()).await;
    {
        let state_mut = Arc::get_mut(&mut state).expect("state Arc 必须 unique");
        state_mut.py_backend = py_backend;
    }
    let app = build_minimal_app(state.clone()).await;
    (app, state)
}

// ===========================================================================
// 1. 未带 token → 40100 UNAUTHORIZED
// ===========================================================================

#[tokio::test]
async fn without_token_returns_40100() {
    let (_pool, app, _fx, _token) = bootstrap().await;

    // 不带 Authorization header 调 /files/sts-tmp-keys
    let req = json_request(
        "POST",
        "/files/sts-tmp-keys",
        Some(json!({
            "uploads": [{"uid": 1, "sha16": "0123456789abcdef"}, {"uid": 2, "sha16": "fedcba9876543210"}]
        })),
        None,
    );
    let (status, env) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "未带 token 应返 401: {env}"
    );
    assert_eq!(
        env["code"], 40100,
        "未带 token 应返 UNAUTHORIZED (40100)，不是 40105"
    );
}

// ===========================================================================
// 2. 带 MANAGER token + MockPyBackend 返回 200 → 200 + body 透传
// ===========================================================================

#[tokio::test]
async fn with_manager_token_and_200_response_forwards_body() {
    let (pool, _app, fx, token) = bootstrap().await;

    // mock：返回 200 + python 信封
    let py_response_body = json!({
        "code": 0,
        "message": "ok",
        "data": {
            "tmp_dir": "tmp/100/",
            "credentials": {
                "tmpSecretId": "AKIDxxxx",
                "tmpSecretKey": "yyyy",
                "sessionToken": "zzz",
                "startTime": 1700000000,
                "expiredTime": 1700001800,
            },
            "allowed_prefix": "tmp/100/",
            "allowed_actions": ["name/cos:PutObject"],
        }
    });
    let resp_body_bytes = Bytes::from(serde_json::to_vec(&py_response_body).unwrap());

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_sts_tmp_keys()
        .times(1)
        .returning(move |_body, _headers| {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", "application/json".parse().unwrap());
            Ok(PyBackendResponse {
                status: StatusCode::OK,
                headers,
                body: resp_body_bytes.clone(),
            })
        });

    let (app2, _state) = make_app_with_mock(&pool, mock).await;

    let req = json_request(
        "POST",
        "/files/sts-tmp-keys",
        Some(json!({
            "uploads": [{"uid": fx.manager_user_id, "sha16": "0123456789abcdef"}]
        })),
        Some(&token),
    );
    let (status, env) = send(app2, req).await;
    assert_eq!(status, StatusCode::OK, "200 应透传: {env}");
    assert_eq!(env["code"], 0, "python 信封 code=0 应透传: {env}");
    assert_eq!(
        env["data"]["tmp_dir"], "tmp/100/",
        "data.tmp_dir 应透传: {env}"
    );
    assert_eq!(
        env["data"]["credentials"]["tmpSecretId"], "AKIDxxxx",
        "credentials.tmpSecretId 应透传: {env}"
    );
}

// ===========================================================================
// 3. 带 MANAGER token + MockPyBackend 返回 502 → 502 + 20406
// ===========================================================================

#[tokio::test]
async fn with_manager_token_and_502_returns_sts_forward_failed() {
    let (pool, _app, _fx, token) = bootstrap().await;

    // mock：返回 502 BAD_GATEWAY（模拟 python 上游不可达 → 上层应报 BIZ_STS_FORWARD_FAILED）
    //
    // 关键设计：handler 端把 py_backend 返回的 AppError 直接透传。MockPyBackend
    // 返回 `AppError::biz(BIZ_STS_FORWARD_FAILED, ...)` 时，handler 调
    // `state.py_backend.forward_sts_tmp_keys(...)` 直接拿到这个 Err；不再二次包装。
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_sts_tmp_keys()
        .times(1)
        .returning(|_body, _headers| {
            Err(AppError::biz(
                hsh_erp_rust::shared::error::code::BIZ_STS_FORWARD_FAILED,
                "MockPyBackend 模拟 python 上游 502",
            ))
        });

    let (app2, _state) = make_app_with_mock(&pool, mock).await;

    let req = json_request(
        "POST",
        "/files/sts-tmp-keys",
        Some(json!({"uploads": []})),
        Some(&token),
    );
    let (status, env) = send(app2, req).await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "BIZ_STS_FORWARD_FAILED 应映射 502: {env}"
    );
    assert_eq!(
        env["code"], 20406,
        "BIZ_STS_FORWARD_FAILED 应返 20406，不是 50001: {env}"
    );
}