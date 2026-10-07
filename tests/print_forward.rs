//! 打印链路 BFF 转发集成测试（2026-10-03 新增）
//!
//! 覆盖 4 条打印端点的「鉴权 + 闸门 + 转发」链路：
//! 1. `POST /com/delivery/note/{id}/print` → 命中 `forward_delivery_note_print`
//! 2. `POST /com/delivery/note/{id}/print-labels` → 命中 `forward_delivery_note_labels`
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
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::py_backend::{
    HttpPyBackend, MockPyBackendClient, PyBackendClient, PyBackendResponse,
};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::shared::error::AppError;
use hsh_erp_test_support::{
    PartFixture, json_request, load_part_fixture, login_token, send as ts_send, test_app,
    test_pool, test_state,
};

// ===========================================================================
// 路径常量（同时是被测路由的字面量）
// ===========================================================================

const DN_PRINT_URI: &str = "/com/delivery/note/1234567890/print";
const DN_LABELS_URI: &str = "/com/delivery/note/1234567890/print-labels";
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
///
/// 2026-10-04 补注：`1234567890` 这个 note 在库里**没有批次**，故套数注入走
/// 「无批次 → 不注入任何键、原样转发」分支，body 仍与入参逐字相等（含前端自己
/// 发的 `merge_quantities`）。下方那条断言把这个分支钉死：一旦有人把注入改成
/// 无条件的（比如凭空造一个空 `merge_quantities` 覆盖掉），本用例会红。
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
        "/com/delivery/note/1234567890/print-preview",
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

/// mock python 响应体 `b"%PDF-mock-python-body"` 的 gzip 流（`mtime=0` 定长输出）。
///
/// 手写常量而非引 `flate2` / `async-compression`（生产侧已是 reqwest 的传递依赖，
/// 测试侧不必为一段固定字节再开一个 dev-dep）。
const MOCK_GZIPPED_BODY: &[u8] = &[
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0x53, 0x0d, 0x70, 0x71, 0xd3, 0xcd,
    0xcd, 0x4f, 0xce, 0xd6, 0x2d, 0xa8, 0x2c, 0xc9, 0xc8, 0xcf, 0xd3, 0x4d, 0xca, 0x4f, 0xa9, 0x04,
    0x00, 0x70, 0x34, 0x7a, 0xcd, 0x15, 0x00, 0x00, 0x00,
];

/// 起本地 mock python 服务端（注册 4 条 v1 路由），返回 (base_url, 捕获槽)。
///
/// 响应刻意做两件事，用来证明 rust 侧的响应头清洗确实生效：
/// - body 用 **stream** 发送 → hyper 走 `transfer-encoding: chunked`、**不发**
///   `content-length`。于是 rust 必须**自己按实际 body 长度补出**
///   `content-length`，否则前端 blob 下载会被截断成「下到一个坏文件且无报错」。
/// - 带 `content-encoding: gzip` 与 `server: uvicorn` —— 这两个头都必须被剥掉。
///
/// `gzip` 头的 body 是**真 gzip 字节**（[`MOCK_GZIPPED_BODY`]，即
/// `b"%PDF-mock-python-body"` 的 gzip 流）：`reqwest` 开了 `gzip` feature，会在解码层
/// 把它还原成明文并摘掉 `content-encoding` / `content-length`，rust 侧
/// `filter_response_headers` 再剥一次 `content-encoding`（防御性兜底）并按明文长度
/// 重算 `content-length`。若这里改成「gzip 头 + 明文 body」，reqwest 解压失败，
/// 整个链路会变成 502，测不到头清洗。
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
        let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> =
            vec![Ok(axum::body::Bytes::from_static(MOCK_GZIPPED_BODY))];
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

// ===========================================================================
// 11. 套数注入：assembly_ids / merge_quantities（2026-10-04 新增）
// ===========================================================================
//
// 打印 handler 不再是纯转发：转发前读本单批次算装配件「可出货套数」，注入 body。
// 公式与边界见 `src/modules/com/delivery_note/service/shippable_sets.rs`，本节锁死
// 对外可见的 4 件事：
// 1. 注入的 id 与 key 全是 JSON **string**（雪花 id > 2^53）；
// 2. 「不注入」分支（本单无装配件 / 装配件全软删）不产生空数组或空对象；
// 3. 套数口径的 4 条边界（凑不齐 = 0 / 超交 LEAST 收口 / quantity=0 子件不参与 /
//    min 取所有子件）；
// 4. 两个端点注入结果逐字一致（python 端 xlsx 行构建是同一份代码）。
//
// seed 约定：测试侧 SQL 一律用 `sqlx::query()`（**不是** `query!` 宏），避免为
// 纯测试 INSERT 往 `.sqlx/` 离线缓存塞新条目（`tests/delivery/note.rs:1404` 同款）。

/// 本文件 seed helper 共用的雪花生成器。
///
/// ⚠️ 必须共享：每次 `SnowflakeIdGenerator::new()` 的首个 id 相同（sequence=0），
/// 各自 `new()` 的 helper 在**同一张表**插两行会直接撞主键。范式
/// `tests/com/union_list.rs::next_test_id`。
static SHARED_SNOWFLAKE: std::sync::OnceLock<SnowflakeIdGenerator> = std::sync::OnceLock::new();

fn next_test_id() -> i64 {
    SHARED_SNOWFLAKE
        .get_or_init(|| SnowflakeIdGenerator::new(1_577_836_800_000, 1))
        .next_id()
}

/// 转发 body 捕获槽：`withf` 同步闭包里写，测试体内读。
type BodySlot = Arc<Mutex<Option<Value>>>;

fn body_slot() -> BodySlot {
    Arc::new(Mutex::new(None))
}

/// `withf` 用：把转发出去的 body 存进槽，返回 true（`times(1)` 另行保证次数）。
fn record_body(slot: &BodySlot, body: &Value) -> bool {
    *slot.lock().expect("body slot mutex") = Some(body.clone());
    true
}

/// 取捕获到的 body。没取到说明转发压根没发生（`times(1)` 会先炸，这里是双保险）。
fn take_body(slot: &BodySlot) -> Value {
    slot.lock()
        .expect("body slot mutex")
        .take()
        .expect("py_backend 转发方法未被调用")
}

/// 装一个 `t_customer` 行（L1：parent_id = NULL；L2：parent_id = l1_id）。
async fn seed_customer(pool: &PgPool, name: &str, parent_id: Option<i64>) -> i64 {
    let id = next_test_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'F', 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(parent_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_customer");
    id
}

/// 装一张 `DRAFT` 送货单（打印 handler 不读单头，但让用例读起来是完整业务场景）。
async fn seed_note(pool: &PgPool, l1_id: i64, no: &str) -> i64 {
    let id = next_test_id();
    sqlx::query(
        "INSERT INTO t_delivery_note \
         (id, delivery_note_no, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'DRAFT', 0, now(), now())",
    )
    .bind(id)
    .bind(no)
    .bind(l1_id)
    .execute(pool)
    .await
    .expect("insert t_delivery_note");
    id
}

/// 装一个装配件工单（`quantity` = 套数）。
async fn seed_assembly(pool: &PgPool, customer_id: i64, name: &str, quantity: i32) -> i64 {
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'ASM-001', $2, '', $3, $4, $4, 'ACTIVE', $5, 0, 0, 0, $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(quantity)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_assembly");
    id
}

/// 装一个工单：`assembly_id = Some(..)` 是装配件子件，`None` 是散件；
/// `quantity` 是**整单数量**（每套需要「整单数量 / 装配件套数」件子）。
async fn seed_part(
    pool: &PgPool,
    customer_id: i64,
    name: &str,
    assembly_id: Option<i64>,
    quantity: i32,
) -> i64 {
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by, assembly_id) \
         VALUES ($1, $2, $3, 'D-001', $4, 'READY_TO_SHIP', $3, $5, $5, $6, 0, \
         $7, NULL, $7, NULL, $8)",
    )
    .bind(id)
    .bind(Option::<String>::None) // serial_no 可空（varchar(15)，别塞雪花 id 进去）
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(quantity)
    .bind(now)
    .bind(assembly_id)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 装一个挂在单上的批次（`quantity` = 本单出货量）。
async fn seed_batch(pool: &PgPool, part_id: i64, note_id: i64, quantity: i32) -> i64 {
    let id = next_test_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, delivery_note_id, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 'READY_TO_SHIP', $4, 0, $5, NULL, $5, NULL)",
    )
    .bind(id)
    .bind(part_id)
    .bind(quantity)
    .bind(note_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 软删一个装配件（`list_by_ids(include_deleted=false)` 解析不到它）。
async fn soft_delete_assembly(pool: &PgPool, asm_id: i64) {
    sqlx::query("UPDATE t_assembly SET deleted_at = now() WHERE id = $1")
        .bind(asm_id)
        .execute(pool)
        .await
        .expect("soft delete t_assembly");
}

fn print_uri(note_id: i64) -> String {
    format!("/com/delivery/note/{note_id}/print")
}

fn labels_uri(note_id: i64) -> String {
    format!("/com/delivery/note/{note_id}/print-labels")
}

/// 装一套「1 个装配件 + 若干子件 + 批次挂单」的场景，返回 (note_id, asm_id, 子件 part id 列表)。
///
/// `children` = `(子件名, 整单数量, 本单出货量)` 三元组。
///
/// 2026-10-04 review 第 3 轮修正：本单出货量改成 `Option<i32>`，**`None` = 该子件
/// 完全不挂批次到本单**（不是「挂一行 quantity = 0 的批次」）。
///
/// ⚠️ 这两件事对「`min` 定义域」类断言是**完全不同**的场景：
/// `Some(0)` 是本单有一行 0 量的批次行 ⇒ 只扫本单行集的旧口径也能算出 0 套 ⇒ 用例绿；
/// `None` 是本单一行都没有 ⇒ 只有扫「全部子件」的口径才会算成 0 套。
/// 原签名只能表达前者，导致「子件 C 不在本单」的两条用例给出的是**虚假保障**
/// （退回 BLOCKER-1 修复前的实现照样全绿）。
async fn seed_assembly_scenario(
    pool: &PgPool,
    asm_quantity: i32,
    children: &[(&str, i32, Option<i32>)],
) -> (i64, i64, Vec<i64>) {
    let l1 = seed_customer(pool, "注入客户", None).await;
    let l2 = seed_customer(pool, "注入二厂", Some(l1)).await;
    let note_id = seed_note(pool, l1, "DN-TEST-9001").await;
    let asm_id = seed_assembly(pool, l1, "注入装配体", asm_quantity).await;
    let mut part_ids = Vec::new();
    for (name, part_qty, note_qty) in children {
        let pid = seed_part(pool, l2, name, Some(asm_id), *part_qty).await;
        if let Some(nq) = note_qty {
            seed_batch(pool, pid, note_id, *nq).await;
        }
        part_ids.push(pid);
    }
    (note_id, asm_id, part_ids)
}

/// 断言「注入的 id 与 map key 全是 JSON string」：值层用 `Value::String` 比对，
/// 再把序列化文本里带引号的形态钉死（雪花 id > 2^53，number 会丢精度）。
fn assert_id_is_json_string(forwarded: &Value, asm_id: i64) {
    let ids = forwarded["assembly_ids"]
        .as_array()
        .expect("assembly_ids 必须是数组");
    assert!(
        ids.iter().all(|v| v.is_string()),
        "assembly_ids 元素必须是 JSON string: {ids:?}"
    );
    assert!(
        forwarded["merge_quantities"]
            .as_object()
            .expect("merge_quantities 必须是对象")
            .keys()
            .all(|k| k.parse::<i64>().is_ok()),
        "merge_quantities 的 key 必须是雪花 id 的十进制字符串"
    );
    let raw = forwarded.to_string();
    assert!(
        raw.contains(&format!("\"{asm_id}\"")),
        "装配件 id 必须以带引号的 string 出现在转发 body 里: {raw}"
    );
}

/// ★ 主路径：有装配件 → 注入正确的 `assembly_ids` / `merge_quantities`。
///
/// 装配件 10 套；子件 A 整单 10 件、本单 8 件（8 套）；子件 B 整单 10 件、本单
/// 5 件（5 套）⇒ min = 5，`LEAST(5, 10)` 仍是 5。
#[tokio::test]
async fn delivery_note_print_injects_shippable_sets_for_assembly() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    let (note_id, asm_id, _parts) =
        seed_assembly_scenario(&pool, 10, &[("子件A", 10, Some(8)), ("子件B", 10, Some(5))]).await;

    let slot = body_slot();
    let s = slot.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |note_id_s, body, _h| note_id_s == note_id.to_string() && record_body(&s, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-asm")));
    // 姊妹方法一次都不能被调用（否则 2 条路由串了）
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, out) = send_bytes(
        app2,
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out, b"xlsx-asm");

    let fwd = take_body(&slot);
    assert_eq!(
        fwd["assembly_ids"],
        json!([asm_id.to_string()]),
        "只注入能解析到的装配件，且序列化为 string"
    );
    let mq = fwd["merge_quantities"]
        .as_object()
        .expect("merge_quantities 必须是对象");
    assert_eq!(mq.len(), 1, "只应有本单这一个装配件: {mq:?}");
    assert!(
        !mq.contains_key("1900000000000000003"),
        "前端自己发的 merge_quantities（人工 override）必须被 rust 侧算出的套数整体覆盖: {mq:?}"
    );
    assert_eq!(
        mq[&asm_id.to_string()],
        json!(5),
        "套数 = min(子件各能撑的套数) = min(8, 5)"
    );
    // 前端字段原样透传（注入是「加键」不是「替换 body」）
    assert_eq!(fwd["custom_order"], snowflake_body()["custom_order"]);
    assert_eq!(fwd["line_item_ids"], snowflake_body()["line_item_ids"]);
    assert_id_is_json_string(&fwd, asm_id);
}

/// 无装配件（纯散件单）→ **不注入**两个键，body 与入参逐字相等。
///
/// ⚠️ `snowflake_body()` 自带一个 `merge_quantities`：本分支下它**原样透传**
/// （handler 只在有装配件时覆盖写该键，不做「无装配件就抹掉」的清洗）。
#[tokio::test]
async fn delivery_note_print_without_assembly_injects_nothing() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    let l1 = seed_customer(&pool, "散件客户", None).await;
    let l2 = seed_customer(&pool, "散件二厂", Some(l1)).await;
    let note_id = seed_note(&pool, l1, "DN-TEST-9002").await;
    let loose = seed_part(&pool, l2, "散件", None, 10).await;
    seed_batch(&pool, loose, note_id, 4).await;

    let slot = body_slot();
    let s = slot.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |_id, body, _h| record_body(&s, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-loose")));
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, _out) = send_bytes(
        app2,
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let fwd = take_body(&slot);
    assert_eq!(
        fwd,
        snowflake_body(),
        "无装配件时不该注入任何键，也不该动前端字段: {fwd}"
    );
}

/// 装配件软删 → 解析不到 → 既不注入 id 也不注入套数（子件在 python 侧按散件行打印）。
#[tokio::test]
async fn delivery_note_print_skips_soft_deleted_assembly() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    // 2026-10-04 review 第 1 轮修正：用 seed 返回的 asm_id。原实现
    // `SELECT id FROM t_assembly LIMIT 1` 只因 part fixture 不含 t_assembly 行才
    // 恰好选中目标行，fixture 一旦加装配体就会静默测错对象。
    let (note_id, asm_id, _parts) =
        seed_assembly_scenario(&pool, 10, &[("子件A", 10, Some(8))]).await;
    soft_delete_assembly(&pool, asm_id).await;

    let slot = body_slot();
    let s = slot.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |_id, body, _h| record_body(&s, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-deleted")));
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, _out) = send_bytes(
        app2,
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let fwd = take_body(&slot);
    assert_eq!(
        fwd,
        snowflake_body(),
        "装配件软删时不该注入（更不能注入空数组 / 空对象）: {fwd}"
    );
}

/// 子件凑不齐整套 → 套数是 **0**（而不是缺键 / 负数），python 侧据此不进 xlsx。
#[tokio::test]
async fn delivery_note_print_injects_zero_sets_when_children_short() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    // 装配件 10 套；子件整单 20 件（每套 2 件），本单只出 1 件 ⇒ 1*10/20 = 0 套
    let (note_id, asm_id, _parts) =
        seed_assembly_scenario(&pool, 10, &[("子件A", 20, Some(1))]).await;

    let slot = body_slot();
    let s = slot.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |_id, body, _h| record_body(&s, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-short")));
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, _out) = send_bytes(
        app2,
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let fwd = take_body(&slot);
    assert_eq!(fwd["assembly_ids"], json!([asm_id.to_string()]));
    assert_eq!(
        fwd["merge_quantities"][asm_id.to_string().as_str()],
        json!(0),
        "凑不齐整套必须显式给 0（缺键会被 python 端回落到默认 1 套）"
    );
}

/// 子件超交（按比例算出 100 套）→ `LEAST` 收口到装配件工单总套数 10。
#[tokio::test]
async fn delivery_note_print_caps_sets_by_assembly_quantity() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    // 装配件 10 套；子件整单 10 件，本单超交 100 件 ⇒ 100 套，收口到 10
    let (note_id, asm_id, _parts) =
        seed_assembly_scenario(&pool, 10, &[("子件A", 10, Some(100))]).await;

    let slot = body_slot();
    let s = slot.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |_id, body, _h| record_body(&s, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-over")));
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, _out) = send_bytes(
        app2,
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let fwd = take_body(&slot);
    assert_eq!(
        fwd["merge_quantities"][asm_id.to_string().as_str()],
        json!(10),
        "子件超交时套数必须被 LEAST 收口到装配件总套数（UI/xlsx 会出现「100 / 10 套」）"
    );
}

/// `part.quantity == 0` 的子件**不参与 min**：既不整除零炸掉，也不把套数拖成 0。
#[tokio::test]
async fn delivery_note_print_ignores_zero_quantity_child_in_min() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    // 装配件 10 套；子件 A 整单 10 件 / 本单 8 件 → 8 套；
    // 子件 B 整单 0 件 / 本单 3 件（越界数据）→ 跳过，min 仍取 8
    let (note_id, asm_id, _parts) =
        seed_assembly_scenario(&pool, 10, &[("子件A", 10, Some(8)), ("子件B", 0, Some(3))]).await;

    let slot = body_slot();
    let s = slot.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |_id, body, _h| record_body(&s, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-zeroqty")));
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, _out) = send_bytes(
        app2,
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "quantity=0 的子件不能让请求 500（对应 SQL 的 NULLIF）"
    );

    let fwd = take_body(&slot);
    assert_eq!(
        fwd["merge_quantities"][asm_id.to_string().as_str()],
        json!(8),
        "quantity=0 的子件不参与 min，套数应取子件 A 的 8 套"
    );
}

/// ★ 两个端点注入结果逐字一致（python 端 xlsx 行构建是同一份 `_prepare_print_rows`，
/// 套数必须同一口径，否则同一单「送货单」与「标签」打出不同套数）。
#[tokio::test]
async fn both_print_endpoints_inject_identical_shippable_sets() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    let (note_id, asm_id, _parts) =
        seed_assembly_scenario(&pool, 10, &[("子件A", 10, Some(8)), ("子件B", 10, Some(5))]).await;

    let print_slot = body_slot();
    let labels_slot = body_slot();
    let (ps, ls) = (print_slot.clone(), labels_slot.clone());
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |_id, body, _h| record_body(&ps, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-print")));
    mock.expect_forward_delivery_note_labels()
        .times(1)
        .withf(move |_id, body, _h| record_body(&ls, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-labels")));

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (s1, _h1, b1) = send_bytes(
        app2.clone(),
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _h2, b2) = send_bytes(
        app2,
        json_request(
            "POST",
            &labels_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b1, b"xlsx-print");
    assert_eq!(b2, b"xlsx-labels");

    let from_print = take_body(&print_slot);
    let from_labels = take_body(&labels_slot);
    assert_eq!(from_print, from_labels, "两个端点注入的 body 必须逐字一致");
    assert_eq!(from_print["assembly_ids"], json!([asm_id.to_string()]));
    assert_eq!(
        from_print["merge_quantities"][asm_id.to_string().as_str()],
        json!(5)
    );
    assert_id_is_json_string(&from_labels, asm_id);
}

/// ★ 2026-10-04 review 第 1 轮（BLOCKER-1）：装配件有子件**本单完全没交批次**时，
/// 套数必须是 0，不能只按「本单出现过的子件」取 min。
///
/// 装配件 10 套；子件 A 整单 10 件 / 本单送 8 件（8 套）；子件 C 整单 10 件 /
/// **本单一行批次都没有**（`note_qty = None`，0 套）⇒ min = 0。业务上剩余部分不能
/// 单独发货、必须等子件收齐，打印时也不能凭空打出 8 套 —— python 端拿到 0 会丢掉
/// 该装配件的全部子件行。
///
/// ⚠️ 2026-10-04 review 第 3 轮（MINOR-1）：本例原先写的是 `("子件C", 10, 0)`，
/// 即给 C 挂了一行 **quantity = 0 的批次**。那种 seed 下旧口径（只扫本单批次行）
/// 也能算出 0 套，用例对 BLOCKER-1 没有任何鉴别力。改成 `None`（C 真不在本单）后，
/// `min` 的定义域里 C 只能来自「全部子件」查询 —— 退回旧实现本例会红。
#[tokio::test]
async fn delivery_note_print_injects_zero_sets_when_child_absent_from_note() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    // 装配件 10 套；子件 A 整单 10 件 / 本单 8 件 → 8 套；
    // 子件 C 整单 10 件 / **本单不挂批次** → 0 套 ⇒ min = 0
    let (note_id, asm_id, _parts) =
        seed_assembly_scenario(&pool, 10, &[("子件A", 10, Some(8)), ("子件C", 10, None)]).await;

    let slot = body_slot();
    let s = slot.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |_id, body, _h| record_body(&s, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-absent")));
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    let (status, _h, _out) = send_bytes(
        app2,
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let fwd = take_body(&slot);
    assert_eq!(
        fwd["assembly_ids"],
        json!([asm_id.to_string()]),
        "装配件本身仍要注入（子件行是散件行兜底还是丢弃由 python 端按套数决定）"
    );
    assert_eq!(
        fwd["merge_quantities"][asm_id.to_string().as_str()],
        json!(0),
        "本单没交批次的子件必须以 0 参与 min：凑不齐整套不能发，否则印出物理上不存在的整套"
    );
}

/// ★ 同源同值（2026-10-04 review 第 1 轮，frontend reviewer 硬要求）：
/// 详情 VO 的 `line_items[].shippable_sets`（前端预览显示的套数）与注入 body 的
/// `merge_quantities[asm_id]`（实际导出 xlsx 的套数）必须**同源同值**。
///
/// 两条链路的输入集必须完全一致（都基于「该装配件的全部子件」，含本单没批次的子件），
/// 否则用户会看到一个数、拿到另一个数的文件，且前端无从发现。用例刻意造
/// 「子件 A 交 8 件 + 子件 C 一件没交」这个只有「全部子件」口径才会算成 0 的场景，
/// 并额外断言两侧都等于 0：若任一侧退回旧口径（只看本单批次行），两侧会同时变成 8，
/// 单纯的「两侧相等」断言察觉不到。
///
/// ⚠️ 2026-10-04 review 第 3 轮（MINOR-1）：子件 C 用 `note_qty = None`（真不在本单，
/// 不是挂一行 0 量批次）。连带后果是 `line_items` 只有子件 A 一行 —— 详情 VO 的
/// `line_items` 由**本单批次行**驱动，C 没有批次就没有行可挂；C 的 0 贡献只体现在
/// 「同一行上的 `shippable_sets` 被压成 0」这件事上。
#[tokio::test]
async fn detail_shippable_sets_match_injected_merge_quantities() {
    let (pool, app, fx) = bootstrap().await;
    let manager_token = token_of(&app, &fx.manager_username).await;
    // 装配件 10 套；子件 A 整单 10 件 / 本单 8 件（8 套）；子件 C 整单 10 件 /
    // **本单不挂批次**（0 套）⇒ 全子件口径下 min = 0
    let (note_id, asm_id, _parts) =
        seed_assembly_scenario(&pool, 10, &[("子件A", 10, Some(8)), ("子件C", 10, None)]).await;

    let slot = body_slot();
    let s = slot.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_delivery_note_print()
        .times(1)
        .withf(move |_id, body, _h| record_body(&s, body))
        .returning(|_i, _b, _h| Ok(ok_xlsx_response(b"xlsx-same-source")));
    mock.expect_forward_delivery_note_labels().never();

    let (app2, _fx) = app_with_mock(&pool, mock).await;
    // ① 详情端点（前端预览的数据源）
    let (ds, denv) = send_json(
        app2.clone(),
        json_request(
            "GET",
            &format!("/com/delivery/note/{note_id}"),
            None,
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(ds, StatusCode::OK, "get detail: {denv}");
    let items = denv["data"]["line_items"]
        .as_array()
        .expect("line_items 必须是数组");
    let detail_sets: Vec<i64> = items
        .iter()
        .filter(|i| i["assembly_id"].as_str() == Some(asm_id.to_string().as_str()))
        .map(|i| {
            i["shippable_sets"]
                .as_i64()
                .unwrap_or_else(|| panic!("装配件子件行必须有 shippable_sets: {i}"))
        })
        .collect();
    assert_eq!(
        detail_sets.len(),
        1,
        "只有子件 A 在本单（子件 C 无批次 ⇒ 无行）；该行仍必须带 assembly_id 与 shippable_sets: {denv}"
    );
    assert!(
        detail_sets.iter().all(|s| *s == 0),
        "详情口径必须是「全部子件」：本单没交批次的子件以 0 参与 min ⇒ 0 套，实际 {detail_sets:?}"
    );

    // ② 打印端点（实际导出 xlsx 的数据源）
    let (ps, _h, _out) = send_bytes(
        app2,
        json_request(
            "POST",
            &print_uri(note_id),
            Some(snowflake_body()),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(ps, StatusCode::OK);
    let fwd = take_body(&slot);
    let injected = fwd["merge_quantities"][asm_id.to_string().as_str()]
        .as_i64()
        .expect("merge_quantities[asm_id] 必须是 JSON number");

    // ③ 同源同值
    assert_eq!(
        detail_sets,
        vec![injected; detail_sets.len()],
        "预览显示的套数（shippable_sets）必须与导出 xlsx 的套数（merge_quantities）相等"
    );
}
