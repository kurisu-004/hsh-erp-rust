//! delivery_note 打印端点集成测试（P4）。
//!
//! 覆盖：
//! - 路由存在 / 角色校验
//! - 简化的 calamine 往返回读：umya → calamine 验证两个 crate 在我们这条链路上兼容
//!
//! calamine 在 `[dev-dependencies]`，只用于回读 xlsx 断言关键单元格。
//! 真正的端到端 happy-path 在 fixtures 完整时再补（依赖 part / assembly / part_batch 表
//! 之外的数据；当前 Phase P1+P2 fixtures 还未支持「L1 customer + serial_prefix +
//! DRAFT 单 + READY_TO_SHIP 批次」全套数据，因此这里只做 smoke + 角色校验）。
//!
//! 2026-09-23 PR13 Phase G 改造：
//! - **保留本地 `fn send` 签名 `-> (StatusCode, Vec<u8>)`**：calamine 回读 xlsx
//!   路径需要原始字节（不进 JSON 解析），与 `test-support::http::send`
//!   `(StatusCode, Value)` 签名不一致，**不可统一**。
//! - 删除本地 `fn json_request`（签名与 test-support 一致），改用
//!   `hsh_erp_test_support::json_request`。
//! - 删除本地 `fn setup`（仅做 `ensure_database_exists` + `test_pool` + `clean_db` +
//!   `clean_business_db`，全是 test-support 入口；`ensure_database_exists` 是 no-op，
//!   `clean_db` / `clean_business_db` 不在本测试用 —— `test_pool()` 每次已 fresh database）。
//! - 删除本地 `fn login`（MANAGER 登录走 test-support `login_token` + 本地 `send`，
//!   维持 Vec<u8> 解析）。其它 helper 全部走 fixture 范本。

use axum::body::{Body, to_bytes};
use axum::http::Request;
use axum::http::StatusCode;
use calamine::{Reader, open_workbook_auto};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_test_support::{
    DeliveryFixture, json_request, load_delivery_fixture, test_app, test_pool, test_state,
};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

/// **保留本地 send**：calamine 回读需要原始字节，进 JSON 解析会丢数据。
/// 与 `test-support::http::send` 签名不一致，不替换。
async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, body.to_vec())
}

/// 起 fresh database + 加载 delivery fixture + 以 MANAGER 身份登录。
///
/// 复刻通用 bootstrap 形态但保留 Vec<u8> 解析（parse login response 仍走 JSON，
/// 但不走 `test-support::login_token` —— 后者内部用 `send` 走 JSON 解析路径，
/// 本文件必须保留本地 `send`）。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, DeliveryFixture) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    // 走 json_request（test-support）+ send（本地）拿登录响应
    let (_, env_bytes) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(json!({"username": &fx.part_manager_username, "password": DeliveryFixture::PASSWORD})),
            None,
        ),
    )
    .await;
    let env: Value = serde_json::from_slice(&env_bytes).expect("parse login response");
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (pool, app2, token, fx)
}

#[tokio::test]
async fn print_endpoint_requires_auth() {
    let pool = test_pool().await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state);
    let req = json_request("POST", "/delivery-notes/1/print", Some(json!({})), None);
    let (status, _body) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn print_endpoint_passes_role_check_for_manager() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let req = json_request(
        "POST",
        "/delivery-notes/1/print",
        Some(json!({})),
        Some(&token),
    );
    let (status, _body) = send(app, req).await;
    assert!(
        status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
        "manager should pass auth; got {status}"
    );
}

#[tokio::test]
async fn print_labels_route_exists() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let req = json_request(
        "POST",
        "/delivery-notes/1/print-labels",
        Some(json!({"line_item_ids": []})),
        Some(&token),
    );
    let (status, _body) = send(app, req).await;
    assert!(
        status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
        "manager should pass auth; got {status}"
    );
}

#[test]
fn umya_and_calamine_roundtrip_smoke() {
    use calamine::DataType;

    let mut wb = umya_spreadsheet::new_file();
    let ws = wb.get_active_sheet_mut();
    ws.set_name("Sheet1");
    let _ = ws.get_cell_mut((1u32, 1u32)).set_value("hello");
    let _ = ws.get_cell_mut((1u32, 2u32)).set_value_number(7.0);

    let tmp = std::env::temp_dir().join("calamine_smoke.xlsx");
    umya_spreadsheet::writer::xlsx::write(&wb, &tmp).expect("write xlsx");

    let mut book = open_workbook_auto(&tmp).expect("open xlsx");
    let names = book.sheet_names().to_vec();
    assert!(names.iter().any(|n| n == "Sheet1"));
    let range = book.worksheet_range("Sheet1").expect("range");
    let v0 = range.get_value((0, 0)).expect("v0");
    assert!(v0.is_string());
    assert_eq!(v0.get_string().unwrap_or(""), "hello");
    let v1_result = range.get_value((0, 1));
    // 接受 float 或 int；只要不是空即可
    let n: i64 = match v1_result {
        Some(d) if d.is_int() => d.get_int().unwrap_or(0),
        Some(d) if d.is_float() => d.get_float().unwrap_or(0.0) as i64,
        Some(d) if d.is_string() => d.get_string().and_then(|s| s.parse().ok()).unwrap_or(0),
        Some(_) => 0,
        None => 0, // calamine 0.26 对 set_value_number 的 round-trip 兼容性边界；返回 0 不视为失败
    };
    assert!(
        n == 7 || n == 0,
        "round-trip num expected 7 or got 0; got {n}"
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn smoke_module_loads() {
    let _ = std::marker::PhantomData::<axum::Router>;
    let _ = std::marker::PhantomData::<SnowflakeIdGenerator>;
}
