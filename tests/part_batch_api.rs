//! part 域 Phase 1（2026-09-13）批次集成测试：1.5 拆分/取消 + 1.6 事件/位置树。
//!
//! 覆盖：
//!   - split_batch: happy path + 数量校验 + 批次守恒不变量
//!   - cancel_batch: happy path + 终态保护
//!   - list_batches: 工单全部活跃批次
//!   - list_events: 工单事件历史
//!   - location_tree: 位置树聚合
//!
//! ## 批次守恒不变量测试
//! `Σ(未删批次.quantity) = t_part.quantity` 必须保持 —— 用 `invariant` 命名空间测试。

#[path = "common/mod.rs"]
mod common;

#[path = "part_api_helpers.rs"]
mod helpers;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{clean_business_db, clean_db, test_pool};

use helpers::*;


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

async fn setup() -> PgPool {
    common::ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    pool
}

// ===========================================================================
//  拆分 / 取消 / 列表
// ===========================================================================

#[tokio::test]
async fn split_batch_happy_path() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 10, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "3",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "split: {env}");
    assert_eq!(env["code"], 0);
    let new_batch_id = env["data"].as_i64().expect("data is i64");
    assert!(new_batch_id > 0);
}

#[tokio::test]
async fn split_batch_invalid_quantity_rejects() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 10, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    // quantity == batch.quantity (不允许，等于整批)
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "10",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "quantity=全量应拒绝: {env}");
    assert_eq!(env["code"], 20111);
}

#[tokio::test]
async fn split_batch_quantity_negative_rejects() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 10, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "-1",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "quantity<0 应拒绝: {env}");
    assert_eq!(env["code"], 20111);
}

// ===== 批次守恒不变量测试 =====

#[tokio::test]
async fn invariant_split_preserves_total_quantity() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 10, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let (app, token, pool_clone) = login_manager(pool.clone(), "admin").await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "3",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "split: {env}");
    // 不变量：Σ quantity == 10
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(quantity), 0)::bigint FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL",
    )
    .bind(pid)
    .fetch_one(&pool_clone)
    .await
    .expect("sum quantity");
    assert_eq!(total, 10, "拆批前后总件数必须守恒 (10=3+7): {env}");
}

#[tokio::test]
async fn cancel_batch_happy_path() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 5, "PENDING").await;
    let version = batch_version(&pool, bid).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/{bid}/cancel"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel-batch: {env}");
    assert_eq!(env["code"], 0);
}

#[tokio::test]
async fn cancel_batch_terminal_protection() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "COMPLETED").await;
    let bid = insert_batch(&pool, pid, 1, 5, "COMPLETED").await;
    let version = batch_version(&pool, bid).await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let body = json!({
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/batches/{bid}/cancel"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "COMPLETED 批次禁止取消: {env}");
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn list_batches_happy_path() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    insert_batch(&pool, pid, 1, 5, "PENDING").await;
    insert_batch(&pool, pid, 2, 3, "PENDING").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request("GET", &format!("/parts/{pid}/batches"), None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list_batches: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"].as_array().expect("data is array");
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn list_events_happy_path() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request("GET", &format!("/parts/{pid}/events"), None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list_events: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"].as_array().expect("data is array");
    assert!(items.is_empty(), "新工单无事件");
}

#[tokio::test]
async fn location_tree_happy_path() {
    let pool = setup().await;
    let (app, token, _pool) = login_manager(pool, "admin").await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts/location-tree", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "location-tree: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("data.items");
    assert!(
        !items.is_empty(),
        "至少返回 OFFICE / PRODUCTION_SHELF 等父节点"
    );
}
