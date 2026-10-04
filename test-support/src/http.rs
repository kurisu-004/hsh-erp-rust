//! HTTP test helpers shared across all integration test binaries.
//!
//! Migrated verbatim from `tests/production/process_chain.rs` (PR13 Phase F, 2026-09-23)
//! so that all 27+ duplicated copies across `tests/*` can be replaced over time
//! via mechanical search-and-replace (replace local `fn send`/`fn json_request`
//! with `use hsh_erp_test_support::{send, json_request};`).
//!
//! ## 三原语
//! - [`json_request`]: build an `axum::http::Request<Body>` with optional JSON
//!   body + optional `Authorization: Bearer <token>` header.
//! - [`send`]: drive an Axum router via oneshot, return `(StatusCode, Value)`.
//! - [`login_token`]: POST `/iam/login` and return the bearer token string.
//!
//! ## 2026-10-04 新增 [`send_raw`]
//! axum 提取器层的 rejection（如 `Query` 反序列化失败）返回的是**纯文本** body，
//! 不走 `R<T>` 信封，`send` 的 JSON 解析会 panic。断言这类 4xx 需用 `send_raw`
//! 拿原始 body 文本。
//!
//! ## 签名契约（与原版一致，便于批量迁移）
//! - `send(app: axum::Router, req: Request<Body>)` —— app 按值，与原
//!   `tests/production/process_chain.rs::send` 逐字一致。
//! - `json_request(method: &str, uri: &str, body: Option<Value>,
//!   bearer: Option<&str>) -> Request<Body>` —— method 用 `&str`（非 `Method`）。
//! - `login_token(app: &axum::Router, username: &str, password: &str) -> String`
//!   —— 新增 helper，内部走 `json_request` + `send`。
//! - `send_raw(app: axum::Router, req: Request<Body>) -> (StatusCode, String)`
//!   —— 与 `send` 同签名同消费语义，body 交还 `String` 而非 `Value`。

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Build an `axum::http::Request<Body>` with an optional JSON body and an optional
/// `Authorization: Bearer <token>` header.
///
/// - `body = None`           → empty body, no `Content-Type`
/// - `body = Some(value)`    → JSON string body, `Content-Type: application/json`
/// - `bearer = None`         → no `Authorization` header
/// - `bearer = Some("...")`  → `Authorization: Bearer ...`
#[allow(dead_code)]
pub fn json_request(
    method: &str,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    builder.body(body).expect("json_request: build request")
}

/// Drive an Axum router with a single request and return `(status, body-as-json)`.
///
/// Panics on transport errors (oneshot) or body-read errors — those signal a
/// genuine test infrastructure bug, not a 4xx/5xx the SUT should produce.
#[allow(dead_code)]
pub async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body_bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let envelope: Value = serde_json::from_slice(&body_bytes).unwrap_or_else(|e| {
        panic!(
            "parse JSON: {e}; raw = {}",
            String::from_utf8_lossy(&body_bytes)
        )
    });
    (status, envelope)
}

/// Drive an Axum router with a single request and return `(status, body-as-raw-text)`.
///
/// 2026-10-04 新增：`send` 的孪生原语，body 交还 `String` 而非 `Value`，用于断言
/// axum 提取器层 rejection 的纯文本 4xx（如 `Query` 反序列化失败），那类响应不
/// 走 `R<T>` 信封、`send` 会在 JSON 解析处 panic。传输层错误仍 panic（基建 bug）。
#[allow(dead_code)]
pub async fn send_raw(app: axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body_bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&body_bytes).into_owned())
}

/// Log a user in via `POST /iam/login` and return the bearer token string.
///
/// Panics with the raw response body if the response is not 200 or the envelope
/// does not contain `data.token` — this is a fixture/auth wiring bug, not a
/// legitimate SUT error.
#[allow(dead_code)]
pub async fn login_token(app: &axum::Router, username: &str, password: &str) -> String {
    let req = json_request(
        "POST",
        "/iam/login",
        Some(json!({ "username": username, "password": password })),
        None,
    );
    let (status, body) = send(app.clone(), req).await;
    if status != StatusCode::OK {
        panic!("login_token: expected 200 OK for user '{username}', got {status} body={body}");
    }
    body.get("data")
        .and_then(|d| d.get("token"))
        .and_then(|t| t.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            panic!("login_token: response missing data.token for user '{username}', body={body}")
        })
}
