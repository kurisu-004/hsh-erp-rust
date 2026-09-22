//! 2026-09-23 新增 Idempotency 中间件集成测试
//!
//! 覆盖 planner T09 决策的 7 个核心用例 + 1 个可选 TTL 用例：
//! 1. 同 key 两次 POST → 第二次字节级同响应
//! 2. Arc<AtomicUsize> 计数器证明 handler 只执行 1 次
//! 3. POST 无 header → handler 调 2 次；Redis 无 `idem:` key
//! 4. 跨方法同 key 撞车（决策 D 简化方案）
//! 5. GET 带 header → handler 调；Redis 无 key
//! 6. DELETE 带 header → handler 调；Redis 无 key
//! 7. 带 key 与不带 key 的 POST 第二次都不命中（仅第一次命中）
//! 8. (可选) TTL=1 → sleep 2s → 再 POST 走 handler
//!
//! 测试栈：tokio::test + tower::ServiceExt::oneshot + 自建 mini router
//! （绕过 auth_middleware，仅挂 idempotency_middleware + 计数器 handler）
//!
//! ⚠️ 注意：测试 binary 名 `idempotency_api` 必须独占 Redis db 16；
//! `tests/common/mod.rs::test_redis_url` 已分配。

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use deadpool_redis::redis::AsyncCommands;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_rust::middleware::idempotency::idempotency_middleware;
use hsh_erp_rust::state::AppState;

// ===========================================================================
// 测试 fixture：计数器 handler + 自建 mini router
// ===========================================================================

/// 全局测试计数器：每个 handler 调用都 +1，跨用例串行（atomic 顺序约束）
/// —— 同一 binary 内 `#[sqlx::test]` 等价于 `--test-threads=1`。
static COUNTER: AtomicUsize = AtomicUsize::new(0);

async fn counted_post(State(_state): State<Arc<AppState>>) -> Response {
    COUNTER.fetch_add(1, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({"echo": "post", "ok": true}))).into_response()
}

async fn counted_put(State(_state): State<Arc<AppState>>) -> Response {
    COUNTER.fetch_add(1, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({"echo": "put", "ok": true}))).into_response()
}

async fn counted_get(State(_state): State<Arc<AppState>>) -> Response {
    COUNTER.fetch_add(1, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({"echo": "get", "ok": true}))).into_response()
}

async fn counted_delete(State(_state): State<Arc<AppState>>) -> Response {
    COUNTER.fetch_add(1, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({"echo": "delete", "ok": true}))).into_response()
}

// 复用 axum::Json 别名
use axum::Json;

/// 自建 mini router：仅挂 idempotency_middleware，不挂 auth_middleware。
///
/// 各 endpoint 用 `Arc<AtomicUsize>` 计数器记录 handler 被调用次数；
/// 这是验证中间件缓存命中的核心手段（命中则 handler 不应被调）。
fn make_test_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/__test/post", post(counted_post))
        .route("/__test/put", put(counted_put))
        .route("/__test/get", get(counted_get))
        .route("/__test/delete", delete(counted_delete))
        .layer(from_fn_with_state(
            state.clone(),
            idempotency_middleware,
        ))
        .with_state(state)
}

// ===========================================================================
// helpers：建测试环境 + 发请求 + 读 Redis 验 key
// ===========================================================================

async fn setup() -> PgPool {
    common::ensure_database_exists().await;
    let pool = common::test_pool().await;
    let redis_pool = common::test_redis_pool().await;
    common::clean_db(&pool).await;
    common::clean_redis(&redis_pool).await;
    pool
}

async fn send(app: Router, req: Request<Body>) -> (StatusCode, Vec<u8>, Response) {
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec();
    // 重建一个空 Response 用于 sanity（不返回 Response；本函数 3 元组为 status/body/cached-resp 占位）
    let _ = headers;
    (status, body, axum::response::Response::new(Body::empty()))
}

fn make_request(
    method: &str,
    uri: &str,
    idem_key: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(k) = idem_key {
        builder = builder.header("idempotency-key", k);
    }
    builder.body(Body::empty()).expect("build request")
}

/// 检查 Redis 是否存在指定 key（简化 helper：返回 bool）
async fn redis_has_key(key: &str) -> bool {
    let redis_pool = common::test_redis_pool().await;
    let mut conn = redis_pool.get().await.expect("redis conn");
    let exists: bool = conn.exists(key).await.expect("redis EXISTS");
    exists
}

/// 列出 Redis 中所有 `idem:` 前缀 key（用于测试 3/5/6 验证「无 key」）
async fn redis_idem_keys() -> Vec<String> {
    let redis_pool = common::test_redis_pool().await;
    let mut conn = redis_pool.get().await.expect("redis conn");
    let keys: Vec<String> = conn
        .keys("idem:*")
        .await
        .expect("redis KEYS idem:*");
    keys
}

// ===========================================================================
// 用例 1：same_key_returns_cached_response
// ===========================================================================

#[tokio::test]
async fn same_key_returns_cached_response() {
    let _pool = setup().await;
    COUNTER.store(0, Ordering::SeqCst);
    let state = common::test_state_with_redis(_pool.clone(), common::test_redis_pool().await);

    let app = make_test_app(state.clone());
    let key = "test-key-1";

    // 第 1 次 POST：handler 调，缓存
    let (s1, b1, _) = send(
        app.clone(),
        make_request("POST", "/__test/post", Some(key)),
    )
    .await;
    // 给 spawn 的 put 一个机会跑完
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 第 2 次 POST：应命中缓存
    let app2 = make_test_app(state);
    let (s2, b2, _) = send(
        app2,
        make_request("POST", "/__test/post", Some(key)),
    )
    .await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b1, b2, "第二次响应应与第一次字节级一致");
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        1,
        "handler 应只被调一次"
    );
}

// ===========================================================================
// 用例 2：handler_invoked_only_once
// ===========================================================================

#[tokio::test]
async fn handler_invoked_only_once() {
    let _pool = setup().await;
    COUNTER.store(0, Ordering::SeqCst);
    let _state = common::test_state_with_redis(_pool.clone(), common::test_redis_pool().await);

    let key = "test-key-2";

    // 连发 3 次相同 key POST
    for _ in 0..3 {
        let app = make_test_app(common::test_state_with_redis(
            _pool.clone(),
            common::test_redis_pool().await,
        ));
        let (s, _, _) = send(
            app,
            make_request("POST", "/__test/post", Some(key)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        1,
        "3 次 POST 调 handler 总次数应 = 1"
    );
}

// ===========================================================================
// 用例 3：post_without_header_passes_through
// ===========================================================================

#[tokio::test]
async fn post_without_header_passes_through() {
    let _pool = setup().await;
    COUNTER.store(0, Ordering::SeqCst);
    let _state = common::test_state_with_redis(_pool.clone(), common::test_redis_pool().await);

    // 第 1 次 POST 无 header
    let app1 = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s1, _, _) = send(app1, make_request("POST", "/__test/post", None)).await;
    // 第 2 次 POST 无 header
    let app2 = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s2, _, _) = send(app2, make_request("POST", "/__test/post", None)).await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        2,
        "无 header 应调 handler 2 次"
    );
    // Redis 无 idem: 前缀 key
    let keys = redis_idem_keys().await;
    assert!(
        keys.is_empty(),
        "无 header 不应写任何 idem: key；got={keys:?}"
    );
}

// ===========================================================================
// 用例 4：cross_method_same_key_collides
// ===========================================================================

#[tokio::test]
async fn cross_method_same_key_collides() {
    let _pool = setup().await;
    COUNTER.store(0, Ordering::SeqCst);
    let _state = common::test_state_with_redis(_pool.clone(), common::test_redis_pool().await);

    let key = "cross-method-key";

    // 第 1 次：POST /__test/post
    let app1 = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s1, b1, _) = send(
        app1,
        make_request("POST", "/__test/post", Some(key)),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(s1, StatusCode::OK);

    // 第 2 次：PUT /__test/put 同 key
    let app2 = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s2, b2, _) = send(
        app2,
        make_request("PUT", "/__test/put", Some(key)),
    )
    .await;

    // 第 1 次 POST 调 post handler；第 2 次 PUT 应命中第 1 次的缓存，不调 put handler
    assert_eq!(s1, s2, "PUT 应返 POST 的 status");
    assert_eq!(b1, b2, "PUT 应返 POST 的 body 字节级");
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        1,
        "仅 POST handler 调了 1 次；PUT handler 应被缓存命中跳过"
    );
}

// ===========================================================================
// 用例 5：get_with_header_is_skipped
// ===========================================================================

#[tokio::test]
async fn get_with_header_is_skipped() {
    let _pool = setup().await;
    COUNTER.store(0, Ordering::SeqCst);
    let _state = common::test_state_with_redis(_pool.clone(), common::test_redis_pool().await);

    let app = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s, _, _) = send(
        app,
        make_request("GET", "/__test/get", Some("any-key")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        1,
        "GET handler 应被调一次"
    );
    let keys = redis_idem_keys().await;
    assert!(
        keys.is_empty(),
        "GET 完全跳过中间件，不写 idem: key；got={keys:?}"
    );
}

// ===========================================================================
// 用例 6：delete_with_header_is_skipped
// ===========================================================================

#[tokio::test]
async fn delete_with_header_is_skipped() {
    let _pool = setup().await;
    COUNTER.store(0, Ordering::SeqCst);
    let _state = common::test_state_with_redis(_pool.clone(), common::test_redis_pool().await);

    let app = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s, _, _) = send(
        app,
        make_request("DELETE", "/__test/delete", Some("any-key")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        1,
        "DELETE handler 应被调一次"
    );
    let keys = redis_idem_keys().await;
    assert!(
        keys.is_empty(),
        "DELETE 完全跳过中间件，不写 idem: key；got={keys:?}"
    );
}

// ===========================================================================
// 用例 7：header_missing_passes_through（key K1 + POST 不带 key 都走 handler）
// ===========================================================================

#[tokio::test]
async fn header_missing_passes_through() {
    let _pool = setup().await;
    COUNTER.store(0, Ordering::SeqCst);
    let _state = common::test_state_with_redis(_pool.clone(), common::test_redis_pool().await);

    let key = "K1";

    // 第 1 次：POST 带 K1（写缓存 + handler 调）
    let app1 = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s1, _, _) = send(app1, make_request("POST", "/__test/post", Some(key))).await;
    assert_eq!(s1, StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 第 2 次：POST 不带 header（pass-through，调 handler）
    let app2 = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s2, _, _) = send(app2, make_request("POST", "/__test/post", None)).await;
    assert_eq!(s2, StatusCode::OK);

    // 第 3 次：POST 带 K1（应命中缓存，不调 handler）
    let app3 = make_test_app(common::test_state_with_redis(
        _pool.clone(),
        common::test_redis_pool().await,
    ));
    let (s3, _, _) = send(app3, make_request("POST", "/__test/post", Some(key))).await;
    assert_eq!(s3, StatusCode::OK);

    // 总 handler 调用次数 = 2（第 1 次 + 第 2 次）
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        2,
        "第 1 次 + 第 2 次无 header 调 handler，第 3 次命中"
    );
    // Redis 应有 idem:K1
    assert!(
        redis_has_key("idem:K1").await,
        "Redis 应有 idem:K1 缓存条目"
    );
}

// ===========================================================================
// 用例 8（可选）：ttl_expiry —— TTL=1 → sleep 2s → 再 POST 走 handler
// ===========================================================================

#[tokio::test]
async fn ttl_expiry() {
    let _pool = setup().await;
    COUNTER.store(0, Ordering::SeqCst);

    // 改 TTL 为 1 秒；其它字段用测试默认
    let mut state = common::test_state_with_redis(_pool.clone(), common::test_redis_pool().await);
    {
        let state_inner = Arc::get_mut(&mut state).expect("state Arc 必须 unique");
        let old_cfg = (*state_inner.config).clone();
        let new_cfg = Arc::new(hsh_erp_rust::infra::config::AppConfig {
            idempotency_ttl_seconds: 1,
            ..old_cfg
        });
        state_inner.config = new_cfg;
    }

    let app1 = make_test_app(state.clone());
    let (s1, _, _) = send(
        app1,
        make_request("POST", "/__test/post", Some("ttl-key")),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // 给 spawn 一点时间把缓存写入 Redis（避免与 TTL 竞态）
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 确认 Redis 已有该 key
    assert!(
        redis_has_key("idem:ttl-key").await,
        "第一次 POST 后 Redis 应有 idem:ttl-key"
    );

    // 等 TTL 过期（TTL=1s，等待 2s 留足余量）
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 确认 Redis key 已过期
    assert!(
        !redis_has_key("idem:ttl-key").await,
        "TTL 过期后 idem:ttl-key 应消失"
    );

    let app2 = make_test_app(state);
    let (s2, _, _) = send(
        app2,
        make_request("POST", "/__test/post", Some("ttl-key")),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        COUNTER.load(Ordering::SeqCst),
        2,
        "TTL 过期后第二次应走 handler"
    );
}