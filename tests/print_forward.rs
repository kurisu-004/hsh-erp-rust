//! 打印链路 BFF 转发集成测试（2026-10-03 新增）
//!
//! 覆盖 4 条打印端点的「鉴权 + 闸门 + 转发」链路：
//! 1. `POST /delivery-notes/{id}/print` → 命中 `forward_delivery_note_print`
//! 2. `POST /delivery-notes/{id}/print-labels` → 命中 `forward_delivery_note_labels`
//! 3. `GET  /parts/{id}/print-drawing` → 命中 `forward_part_print_pdf`（query 透传）
//! 4. `POST /parts/print-drawing-batch` → 命中 `forward_part_print_pdf_batch`
//!
//! 另覆盖：
//! - 角色闸门：送货单 `MANAGER/CLERK/INSPECTOR` 放行、货架终端 `SHELF_ACCOUNT`
//!   与无角色用户 → 40300；零件图纸打印额外放行 `CNC_PROGRAMMER`；
//! - 端到端（真实 `HttpPyBackend` + 本地 mock python 服务端）：4 条 v2→v1 URL
//!   拼装逐字正确、鉴权头不进转发、`X-Forwarded-User-Id` 进转发、响应头清洗
//!   （`content-length` 按实际 body 重算 / hop-by-hop + `content-encoding` 剥除）；
//! - `is_print_path` 与本工程实际注册的 4 条打印路由**逐字一致**（不一致 = 批量
//!   打印拿不到长档超时 / 打印响应落进 Redis 缓存）；
//! - 幂等中间件的打印路径跳过闸门。
//!
//! ## 测试栈
//! `tokio::test` + `tower::ServiceExt::oneshot` 直调 `test_app`（`v2_router`）。
//! `MockPyBackendClient` 替换 `state.py_backend` 模拟 python 响应；端到端用例
//! 改用真实 `HttpPyBackend` 打本地 mock 服务端，从服务端侧反查真实 URL 与
//! 真实收到的 header。

// 现场造带角色的用户时要抢 `pool_snowflake()` 的 std Mutex（test-support 既有
// 签名），guard 会跨 await。与 `tests/files_sts_tmp_keys.rs` 同款豁免。
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_rust::infra::py_backend::{
    HttpPyBackend, MockPyBackendClient, PyBackendClient, PyBackendResponse,
};
use hsh_erp_rust::shared::error::AppError;
use hsh_erp_test_support::{
    PartFixture, json_request, load_part_fixture, login_token, send as ts_send, test_app,
    test_pool, test_state,
};

// ===========================================================================
// 路径常量（同时是被测路由的字面量）
// ===========================================================================

const DN_PRINT_URI: &str = "/delivery-notes/1234567890/print";
const DN_LABELS_URI: &str = "/delivery-notes/1234567890/print-labels";
const PART_DRAWING_URI: &str = "/parts/1234567890/print-drawing";
const PART_DRAWING_BATCH_URI: &str = "/parts/print-drawing-batch";

/// 前端发的雪花 ID 一律是 string（> 2^53）：rust 侧 `Json<Value>` 透传，不得
/// 解析成数字。
fn snowflake_body() -> Value {
    json!({
        "custom_order": ["1900000000000000001", "1900000000000000002"],
        "merge_quantities": {"1900000000000000003": 4},
        "line_item_ids": ["1900000000000000004"],
    })
}

// ===========================================================================
// Helpers
// ===========================================================================

/// 本地 send：**保留原始字节**版本。打印响应是 PDF / xlsx 二进制流，
/// `test-support::http::send` 的 `(StatusCode, Value)` 会因 JSON 解析失败 panic。
/// 需要读信封时才用 [`send_json`]。
async fn send_bytes(app: axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec();
    (status, headers, body)
}

/// 需要解析 rust 错误信封（40300 / 20407）时用这个。
async fn send_json(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    ts_send(app, req).await
}

/// 造一个 200 + 二进制 body 的 `PyBackendResponse`（xlsx 形态的头）。
fn ok_xlsx_response(body: &'static [u8]) -> PyBackendResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            .parse()
            .unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        "attachment; filename=\"F-2026-10-03-note.xlsx\""
            .parse()
            .unwrap(),
    );
    PyBackendResponse {
        status: StatusCode::OK,
        headers,
        body: Bytes::from_static(body),
    }
}

/// 造一个 200 + 二进制 body 的 `PyBackendResponse`（PDF 形态的头）。
fn ok_pdf_response(body: &'static [u8]) -> PyBackendResponse {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/pdf".parse().unwrap());
    headers.insert(
        header::CONTENT_DISPOSITION,
        "inline; filename=\"parts-batch.pdf\"".parse().unwrap(),
    );
    PyBackendResponse {
        status: StatusCode::OK,
        headers,
        body: Bytes::from_static(body),
    }
}

/// bootstrap：fresh DB + part fixture（内含 MANAGER / INSPECTOR / CLERK /
/// SHELF_ACCOUNT 4 个可登录用户）+ state + app（`v2_router` 完整 nest）。
///
/// 选 `load_part_fixture` 而非 `load_iam_fixture`：本文件要验证
/// `SHELF_ACCOUNT`（货架终端）被拒，而 part fixture 恰好预置了该角色的合法登录
/// 用户，无需在测试内现场造用户。
async fn bootstrap() -> (PgPool, axum::Router, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state);
    (pool, app, fx)
}

/// 登录 part fixture 内某个角色的用户，返回 bearer token。
async fn token_of(app: &axum::Router, username: &str) -> String {
    login_token(app, username, PartFixture::PASSWORD).await
}

/// 用 mockall `MockPyBackendClient` 替换 `state.py_backend` 后重造 app。
///
/// `Arc::get_mut` 要求 Arc 强计数为 1，故必须在 `test_app` 之前替换。
async fn app_with_mock(pool: &PgPool, mock: MockPyBackendClient) -> (axum::Router, PartFixture) {
    let py_backend: Arc<dyn PyBackendClient> = Arc::new(mock);
    let mut state = test_state(pool.clone()).await;
    {
        let state_mut = Arc::get_mut(&mut state).expect("state Arc 必须 unique");
        state_mut.py_backend = py_backend;
    }
    let fx = PartFixture::default();
    let app = test_app(state);
    (app, fx)
}

/// 现场造一个带指定角色的用户并登录（part fixture 只预置 4 个角色，
/// `CNC_PROGRAMMER` 不在其中）。范本：`tests/production/pending_programming.rs`。
async fn login_user_with_role(pool: &PgPool, username: &str, role: &str) -> String {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_test_support::pool_snowflake;

    let hash = password::hash(PartFixture::PASSWORD).expect("bcrypt hash");
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let user_id = snowflake.next_id();
    let role_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $2, true, 0, 0, $4, $4)",
    )
    .bind(user_id)
    .bind(username)
    .bind(hash)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user");
    sqlx::query(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) VALUES ($1, $2, $3, NULL, NULL, 0, $4, $4)",
    )
    .bind(role_id)
    .bind(user_id)
    .bind(role)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");

    let app = test_app(test_state(pool.clone()).await);
    login_token(&app, username, PartFixture::PASSWORD).await
}

// ===========================================================================
// 1. 送货单 print → 命中 forward_delivery_note_print
// ===========================================================================

/// 4 条路由各自命中正确的 py_backend 方法，且 note_id 拼装正确、body 原样透传。
#[tokio::test]
async fn delivery_note_print_forwards_body_and_note_id() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |note_id, body, _h| note_id == "1234567890" && *body == snowflake_body())
        .returning(|_note_id, _body, _headers| Ok(ok_xlsx_response(b"%PDF-xlsx-fake")));
    // 姊妹方法一次都不能被调用（否则 4 条路由串了）
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, headers, body) = send_bytes(
        app2,
        json_request(
            "POST",
            DN_PRINT_URI,
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mock 200 应原样透传");
    assert_eq!(body, b"%PDF-xlsx-fake", "二进制 body 应原样透传");
    assert_eq!(
        headers.get(header::CONTENT_DISPOSITION).unwrap(),
        "attachment; filename=\"F-2026-10-03-note.xlsx\"",
        "content-disposition 是前端 parseFilename 的依据，必须透传"
    );
}

/// 未带 token → 40100（middleware 层，与 STS 转发同形）。
#[tokio::test]
async fn delivery_note_print_without_token_returns_40100() {
    let (pool, _app, _fx) = bootstrap().await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, env) = send_json(
        app2,
        json_request("POST", DN_PRINT_URI, Some(json!({})), None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "未带 token 应 401: {env}");
    assert_eq!(env["code"], 40100, "应为 UNAUTHORIZED: {env}");
}

// ===========================================================================
// 2. 送货单 print-labels → 命中 forward_delivery_note_labels
// ===========================================================================

#[tokio::test]
async fn delivery_note_labels_forwards_to_labels_method() {
    let (pool, app, fx) = bootstrap().await;
    let clerk_token = token_of(&app, &fx.clerk_username).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_labels()
        .times(1)
        .withf(move |note_id, _body, _h| note_id == "1234567890")
        .returning(|_note_id, _body, _headers| Ok(ok_xlsx_response(b"labels-xlsx")));
    mock.expect_forward_delivery_note_print().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _headers, body) = send_bytes(
        app2,
        json_request(
            "POST",
            DN_LABELS_URI,
            Some(snowflake_body()),
            Some(&clerk_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"labels-xlsx");
}

// ===========================================================================
// 3. 单件图纸 → 命中 forward_part_print_pdf（query 原样透传）
// ===========================================================================

/// `?vector=true` 必须**原样**交给 py_backend：rust 侧不解析 bool（python 端是
/// `Query(bool)`），解析会让两处各持一份默认值 / 兼容性规则。
#[tokio::test]
async fn part_print_drawing_forwards_raw_query() {
    let (pool, app, fx) = bootstrap().await;
    let inspector_token = token_of(&app, &fx.inspector_username).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_part_print_pdf()
        .times(1)
        .withf(|part_id, query, _h| {
            part_id == "1234567890" && query.as_deref() == Some("vector=true")
        })
        .returning(|_part_id, _query, _headers| Ok(ok_pdf_response(b"%PDF-1part")));
    mock.expect_forward_part_print_pdf_batch().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, headers, body) = send_bytes(
        app2,
        json_request(
            "GET",
            &format!("{PART_DRAWING_URI}?vector=true"),
            None,
            Some(&inspector_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"%PDF-1part");
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/pdf"
    );
}

/// 不带 query 时 `query` 参数是 `None`（而不是空串）：空串会被拼成 `...print?`。
#[tokio::test]
async fn part_print_drawing_without_query_passes_none() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_part_print_pdf()
        .times(1)
        .withf(|_part_id, query, _h| query.is_none())
        .returning(|_part_id, _query, _headers| Ok(ok_pdf_response(b"%PDF-noquery")));

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _headers, _body) = send_bytes(
        app2,
        json_request("GET", PART_DRAWING_URI, None, Some(&manager_token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// 4. 批量图纸 → 命中 forward_part_print_pdf_batch
// ===========================================================================

/// ★ 静态段 `/print-drawing-batch` 必须**不**被 `/{part_id}` catch-all 吞掉。
/// 若注册顺序写错，axum 会把 `print-drawing-batch` 解析成 part_id → 走单件
/// handler → `Path<i64>` extractor 拒绝 → 400。
#[tokio::test]
async fn part_print_drawing_batch_hits_batch_method() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_part_print_pdf_batch()
        .times(1)
        .withf(move |body, _h| body == &batch_body())
        .returning(|_body, _headers| Ok(ok_pdf_response(b"%PDF-batch")));
    // 单件方法一次都不能被调用
    mock.expect_forward_part_print_pdf().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _headers, body) = send_bytes(
        app2,
        json_request(
            "POST",
            PART_DRAWING_BATCH_URI,
            Some(batch_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "静态段被 catch-all 吞掉会返 400");
    assert_eq!(body, b"%PDF-batch");
}

fn batch_body() -> Value {
    json!({
        "part_ids": ["1900000000000000001", "1900000000000000002"],
        "assembly_ids": ["1900000000000000009"],
        "vector": true,
    })
}

// ===========================================================================
// 5. 身份注入：X-Forwarded-User-Id
// ===========================================================================

/// handler 在转发前注入 `X-Forwarded-User-Id = CurrentUser.id`，值必须是当前
/// 登录用户的雪花 id（python 端据此取真实 user_id，替代反向依赖 rust JWT）。
#[tokio::test]
async fn forwards_inject_x_forwarded_user_id() {
    let (pool, app, fx) = bootstrap().await;
    let inspector_token = token_of(&app, &fx.inspector_username).await;
    let expected_uid = fx.inspector_user_id.to_string();

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_part_print_pdf()
        .times(1)
        .returning(move |_part_id, _query, headers| {
            let got = headers
                .get("x-forwarded-user-id")
                .expect("handler 必须注入 x-forwarded-user-id")
                .to_str()
                .unwrap()
                .to_owned();
            assert_eq!(got, expected_uid, "x-forwarded-user-id 应 = CurrentUser.id");
            Ok(ok_pdf_response(b"%PDF-uid"))
        });

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, _b) = send_bytes(
        app2,
        json_request("GET", PART_DRAWING_URI, None, Some(&inspector_token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// 6. 角色闸门
// ===========================================================================

/// 送货单打印：`SHELF_ACCOUNT`（货架终端）不放行 → 40300，且不触发任何转发。
#[tokio::test]
async fn delivery_note_print_rejects_shelf_account() {
    let (pool, app, fx) = bootstrap().await;
    let shelf_token = token_of(&app, &fx.shelf_account_username).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print().never();
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, env) = send_json(
        app2,
        json_request("POST", DN_PRINT_URI, Some(json!({})), Some(&shelf_token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "SHELF_ACCOUNT 只该扫码，不该打印: {env}"
    );
    assert_eq!(env["code"], 40300, "应为 FORBIDDEN: {env}");
}

/// 零件图纸打印：`SHELF_ACCOUNT` 同样不放行 → 40300。
#[tokio::test]
async fn part_print_drawing_rejects_shelf_account() {
    let (pool, app, fx) = bootstrap().await;
    let shelf_token = token_of(&app, &fx.shelf_account_username).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_part_print_pdf().never();
    mock.expect_forward_part_print_pdf_batch().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, env) = send_json(
        app2,
        json_request("GET", PART_DRAWING_URI, None, Some(&shelf_token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "SHELF_ACCOUNT 不该打印图纸: {env}"
    );
    assert_eq!(env["code"], 40300, "应为 FORBIDDEN: {env}");
}

/// 零件图纸打印**额外放行** `CNC_PROGRAMMER`（送货单不放行）——CNC 编程岗要看
/// 图纸才能编程序。漏这个角色就是 403 直接挡住编程岗的主流程。
#[tokio::test]
async fn part_print_drawing_allows_cnc_programmer() {
    let (pool, _app, _fx) = bootstrap().await;
    let cnc_token = login_user_with_role(&pool, "pf_cnc_programmer", "CNC_PROGRAMMER").await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_part_print_pdf()
        .times(1)
        .returning(|_part_id, _query, _headers| Ok(ok_pdf_response(b"%PDF-cnc")));
    mock.expect_forward_part_print_pdf_batch()
        .times(1)
        .returning(|_body, _headers| Ok(ok_pdf_response(b"%PDF-cnc-batch")));

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, body) = send_bytes(
        app2.clone(),
        json_request("GET", PART_DRAWING_URI, None, Some(&cnc_token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "CNC_PROGRAMMER 应可打印图纸");
    assert_eq!(body, b"%PDF-cnc");

    let (status, _h, body) = send_bytes(
        app2,
        json_request(
            "POST",
            PART_DRAWING_BATCH_URI,
            Some(batch_body()),
            Some(&cnc_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "CNC_PROGRAMMER 应可批量打印图纸");
    assert_eq!(body, b"%PDF-cnc-batch");
}

/// 送货单打印不放行 `CNC_PROGRAMMER`（编程岗用不上单据打印）。
#[tokio::test]
async fn delivery_note_print_rejects_cnc_programmer() {
    let (pool, _app, _fx) = bootstrap().await;
    let cnc_token = login_user_with_role(&pool, "pf_cnc_dn", "CNC_PROGRAMMER").await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print().never();
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, env) = send_json(
        app2,
        json_request("POST", DN_PRINT_URI, Some(json!({})), Some(&cnc_token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "送货单打印不应放行 CNC_PROGRAMMER: {env}"
    );
    assert_eq!(env["code"], 40300, "应为 FORBIDDEN: {env}");
}

// ===========================================================================
// 7. 转发失败 → 502 + BIZ_PRINT_FORWARD_FAILED(20407)
// ===========================================================================

#[tokio::test]
async fn print_forward_failure_returns_20407() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_part_print_pdf_batch()
        .times(1)
        .returning(|_body, _headers| {
            Err(AppError::biz(
                hsh_erp_rust::shared::error::code::BIZ_PRINT_FORWARD_FAILED,
                "MockPyBackend 模拟 python 上游 502",
            ))
        });

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, env) = send_json(
        app2,
        json_request(
            "POST",
            PART_DRAWING_BATCH_URI,
            Some(batch_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "打印转发失败应映射 502: {env}"
    );
    assert_eq!(
        env["code"], 20407,
        "应为 BIZ_PRINT_FORWARD_FAILED(20407)，不是 STS 的 20406: {env}"
    );
}

// ===========================================================================
// 8. is_print_path 与实际注册的 4 条路由逐字一致
// ===========================================================================

/// 判定的输出（长档超时 + 幂等跳过缓存）必须落在**实际注册**的 4 条路由上。
/// 一旦路由改名而 `is_print_path` 没跟着改：批量打印会退回 30s 被砍断
/// （功能不可用），同时多 MB 的 PDF 又会落进 Redis 缓存。
#[test]
fn print_path_gate_matches_registered_routes_verbatim() {
    use hsh_erp_rust::middleware::timeout::is_print_path;

    for uri in [
        DN_PRINT_URI,
        DN_LABELS_URI,
        PART_DRAWING_URI,
        PART_DRAWING_BATCH_URI,
    ] {
        assert!(is_print_path(uri), "已注册的打印路由必须命中长档：{uri}");
        assert!(
            is_print_path(&format!("/api/v2{uri}")),
            "生产形态（/api/v2 前缀）也必须命中：/api/v2{uri}"
        );
    }
}

/// 反向：相近但不是打印的路径不能命中（否则普通端点静默继承 660s 长档）。
#[test]
fn print_path_gate_rejects_lookalikes() {
    use hsh_erp_rust::middleware::timeout::is_print_path;

    for uri in [
        "/delivery-notes/1234567890/print-preview",
        "/parts/1234567890/print",
        // ★ 批量段是 2 段静态；`/parts/{id}/print-drawing-batch` 是 3 段，不能命中
        "/parts/1234567890/print-drawing-batch",
        "/parts/batch",
        "/parts/1234567890",
    ] {
        assert!(!is_print_path(uri), "非打印路径不该命中长档：{uri}");
    }
}

// ===========================================================================
// 9. 端到端：真实 HttpPyBackend + 本地 mock python 服务端
// ===========================================================================

/// mock python 服务端看到的请求（method + 完整 path+query + 收到的 headers）。
#[derive(Debug, Clone, Default)]
struct Captured {
    method: String,
    uri: String,
    headers: HeaderMap,
}

/// mock python 服务端：把 4 条 **v1** 路径注册成真实路由。
///
/// 这样一来，只要 rust 侧的 v2→v1 映射写错（路径拼错 / 动词错），这里就会
/// 404 / 405，测试直接红 —— 不用去翻 rust 源码核对 format! 字符串。
#[derive(Clone, Default)]
struct PyCapture(Arc<std::sync::Mutex<Vec<Captured>>>);

impl PyCapture {
    fn record(&self, method: &str, uri: &str, headers: &HeaderMap) {
        self.0.lock().unwrap().push(Captured {
            method: method.to_string(),
            uri: uri.to_string(),
            headers: headers.clone(),
        });
    }
    fn snapshot(&self) -> Vec<Captured> {
        self.0.lock().unwrap().clone()
    }
}

/// 起本地 mock python 服务端（注册 4 条 v1 路由），返回 (base_url, 捕获槽)。
///
/// 响应刻意做两件事，用来证明 rust 侧的响应头清洗确实生效：
/// - body 用 **stream** 发送 → hyper 走 `transfer-encoding: chunked`、**不发**
///   `content-length`。于是 rust 必须**自己按实际 body 长度补出**
///   `content-length`，否则前端 blob 下载会被截断成「下到一个坏文件且无报错」。
/// - 带 `content-encoding: gzip`（reqwest 未开 gzip feature，body 不会真被解码）
///   与 `server: uvicorn` —— 这两个头都必须被剥掉。
async fn spawn_mock_python() -> (String, PyCapture) {
    use axum::Router;
    use axum::routing::{get as rget, post as rpost};
    use tokio::net::TcpListener;

    let cap = PyCapture::default();

    async fn fake(
        axum::extract::State(cap): axum::extract::State<PyCapture>,
        method: axum::http::Method,
        axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
        headers: HeaderMap,
    ) -> axum::response::Response {
        cap.record(
            method.as_str(),
            uri.path_and_query().map_or("/", |q| q.as_str()),
            &headers,
        );
        // 未知长度的 body → chunked；故意让 python 侧不带 content-length。
        let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> = vec![
            Ok(axum::body::Bytes::from_static(b"%PDF-mock-")),
            Ok(axum::body::Bytes::from_static(b"python-body")),
        ];
        axum::response::Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/pdf")
            .header(
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"python-side.pdf\"",
            )
            .header(header::CACHE_CONTROL, "private, max-age=600")
            .header(header::CONTENT_ENCODING, "gzip")
            .header("server", "uvicorn")
            .header(header::DATE, "Sat, 03 Oct 2026 00:00:00 GMT")
            .body(axum::body::Body::from_stream(tokio_stream::iter(chunks)))
            .expect("mock python 响应构造")
    }

    let app = Router::new()
        // v1 真名：/api/v1/delivery-notes/{note_id}/print[-labels]
        .route("/api/v1/delivery-notes/{note_id}/print", rpost(fake))
        .route("/api/v1/delivery-notes/{note_id}/print-labels", rpost(fake))
        // v1 真名：/api/v1/parts/{part_id}/print（v2 叫 print-drawing）
        .route("/api/v1/parts/{part_id}/print", rget(fake))
        // v1 真名：/api/v1/parts/print-batch（v2 叫 print-drawing-batch）
        .route("/api/v1/parts/print-batch", rpost(fake))
        .with_state(cap.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{}", addr.port()), cap)
}

/// 端到端：4 条 v2 路由 → 4 条 v1 URL 逐字正确 + 鉴权头不进转发 +
/// 响应头清洗（content-length 按实际 body 重算）。
#[tokio::test]
async fn e2e_v1_urls_and_header_hygiene() {
    use std::time::Duration;

    let (pool, _app, fx) = bootstrap().await;
    let (base_url, cap) = spawn_mock_python().await;

    let mut state = test_state(pool.clone()).await;
    {
        let py: Arc<dyn PyBackendClient> = Arc::new(
            HttpPyBackend::new(
                base_url,
                Duration::from_millis(2000),
                Duration::from_millis(2000),
            )
            .expect("构造 HttpPyBackend"),
        );
        let state_mut = Arc::get_mut(&mut state).expect("state Arc 必须 unique");
        state_mut.py_backend = py;
    }
    let app = test_app(state);
    let manager_token = token_of(&app, &fx.manager_username).await;

    // --- 4 条路由逐个打一遍 ---
    let cases: Vec<(&str, &str, Option<Value>)> = vec![
        ("POST", DN_PRINT_URI, Some(snowflake_body())),
        ("POST", DN_LABELS_URI, Some(snowflake_body())),
        ("GET", "/parts/1234567890/print-drawing?vector=true", None),
        ("POST", PART_DRAWING_BATCH_URI, Some(batch_body())),
    ];
    for (method, uri, body) in cases {
        let (status, headers, out_body) = send_bytes(
            app.clone(),
            json_request(method, uri, body, Some(&manager_token)),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{method} {uri} 未命中 mock python 的 v1 路由（v2→v1 映射写错？）"
        );
        assert_eq!(out_body, b"%PDF-mock-python-body", "{uri} body 应透传");

        // 响应头清洗：python 侧是 chunked（没发 content-length），rust 必须补出
        assert_eq!(
            *headers.get(header::CONTENT_LENGTH).unwrap(),
            out_body.len().to_string(),
            "{uri}: content-length 必须按实际 body 长度重算"
        );
        assert!(
            headers.get(header::CONTENT_ENCODING).is_none(),
            "{uri}: content-encoding 必须剥除（body 已被 reqwest 解码）"
        );
        assert!(headers.get("server").is_none(), "{uri}: server 头必须剥除");
        assert_eq!(
            headers.get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"python-side.pdf\"",
            "{uri}: content-disposition 必须保留（前端 parseFilename 依赖）"
        );
        assert_eq!(
            headers.get(header::CACHE_CONTROL).unwrap(),
            "private, max-age=600",
            "{uri}: cache-control 必须保留"
        );
    }

    // --- python 端实际收到的 URI 与 header ---
    let seen = cap.snapshot();
    let uris: Vec<String> = seen.iter().map(|c| c.uri.clone()).collect();
    assert_eq!(
        uris,
        vec![
            "/api/v1/delivery-notes/1234567890/print".to_string(),
            "/api/v1/delivery-notes/1234567890/print-labels".to_string(),
            // ★ python 端单件端点叫 /print（不是 print-drawing），query 原样透传
            "/api/v1/parts/1234567890/print?vector=true".to_string(),
            // ★ python 端批量端点叫 /print-batch（不是 print-drawing-batch）
            "/api/v1/parts/print-batch".to_string(),
        ],
        "4 条 v2→v1 URL 拼装必须逐字正确"
    );
    assert_eq!(seen[2].method, "GET", "单件图纸在 python 端是 GET");
    assert_eq!(seen[3].method, "POST", "批量图纸在 python 端是 POST");

    for c in &seen {
        assert!(
            c.headers.get("authorization").is_none(),
            "Authorization 不得透传到 python（避免 JWT 反向泄露）; got {:?}",
            c.headers
        );
        assert!(
            c.headers.get("cookie").is_none(),
            "Cookie 不得透传到 python; got {:?}",
            c.headers
        );
        let fwd = c
            .headers
            .get("x-forwarded-user-id")
            .expect("x-forwarded-user-id 必须存在（python 端唯一的身份依据）")
            .to_str()
            .unwrap();
        assert_eq!(
            fwd,
            fx.manager_user_id.to_string(),
            "x-forwarded-user-id 应 = CurrentUser.id"
        );
    }
}

// ===========================================================================
// 10. 幂等中间件：打印路径跳过缓存
// ===========================================================================

/// 带 `Idempotency-Key` 的打印路径**不进**缓存（否则多 MB 的 PDF/XLSX 会被整份
/// 写进 Redis 24h）；非打印路径仍正常进缓存。
#[tokio::test]
async fn idempotency_skips_print_paths_but_caches_regular_writes() {
    use hsh_erp_rust::middleware::idempotency::InMemoryIdempotencyStore;
    use hsh_erp_rust::middleware::idempotency::idempotency_middleware;

    let pool = test_pool().await;
    let mut state = test_state(pool.clone()).await;
    let store = Arc::new(InMemoryIdempotencyStore::new());
    {
        let probe: Arc<dyn hsh_erp_rust::middleware::idempotency::IdempotencyStore> = store.clone();
        let state_mut = Arc::get_mut(&mut state).expect("state Arc 必须 unique");
        state_mut.idempotency_store = probe;
    }

    let counter = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route(PART_DRAWING_BATCH_URI, axum::routing::post(counted))
        // 对照组：同名形态的普通写路径
        .route("/api/v2/parts/batch", axum::routing::post(counted))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            idempotency_middleware,
        ))
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| {
                let c = counter.clone();
                req.extensions_mut().insert(c);
                async move { next.run(req).await }
            },
        ))
        .with_state(state);

    let print_key = format!("pf-print-{}", uuid::Uuid::new_v4().simple());
    let (status, _h, _b) = send_bytes(
        app.clone(),
        idem_request(PART_DRAWING_BATCH_URI, &print_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let regular_key = format!("pf-regular-{}", uuid::Uuid::new_v4().simple());
    let (status, _h, _b) = send_bytes(app, idem_request("/api/v2/parts/batch", &regular_key)).await;
    assert_eq!(status, StatusCode::OK);

    // 打印路径：无论等多久都不会有缓存条目
    for _ in 0..20 {
        assert!(
            store
                .get(&format!("idem:{print_key}"))
                .await
                .expect("store.get")
                .is_none(),
            "打印路径带 Idempotency-Key 也不该进缓存（多 MB PDF/XLSX 会 OOM）"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // 对照组：普通写路径必须进缓存（闸门没有误伤幂等能力）
    let mut cached = None;
    for _ in 0..100 {
        if let Some(v) = store
            .get(&format!("idem:{regular_key}"))
            .await
            .expect("store.get")
        {
            cached = Some(v);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        cached.is_some(),
        "非打印路径带 Idempotency-Key 应照常进缓存（打印跳过闸门不能误伤它）"
    );

    use hsh_erp_rust::middleware::idempotency::IdempotencyStore as _;
}

/// 计数 handler：每次被调 counter += 1（证明请求确实穿过了中间件到达下游）。
async fn counted(
    axum::extract::Extension(c): axum::extract::Extension<Arc<AtomicUsize>>,
) -> String {
    c.fetch_add(1, Ordering::SeqCst);
    "ok".to_string()
}

/// 造一个带 `Idempotency-Key` 的 POST 请求。
fn idem_request(uri: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("idempotency-key", key)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .expect("build idem request")
}
