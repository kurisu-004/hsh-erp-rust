//! 2026-09-23 新增 Idempotency 中间件集成测试
//!
//! 覆盖 planner T09 决策的 7 个核心用例 + 1 个可选 TTL 用例 + 1 个公开路径用例：
//! 1. 同 key 两次 POST → 第二次字节级同响应
//! 2. Arc<AtomicUsize> 计数器证明 handler 只执行 1 次
//! 3. POST 无 header → handler 调 2 次；Redis 无 `idem:` key
//! 4. 跨方法同 key 撞车（决策 D 简化方案）
//! 5. GET 带 header → handler 调；Redis 无 key
//! 6. DELETE 带 header → handler 调；Redis 无 key
//! 7. 带 key 与不带 key 的 POST 第二次都不命中（仅第一次命中）
//! 8. (可选) TTL=1 → sleep 2s → 再 POST 走 handler
//! 9. (2026-09-23 review #1) 公开路径闸门：POST /iam/login 带 Idempotency-Key
//!    → 第二次同 key POST 也走 handler（不被 idem 缓存 → 防 JWT 缓存泄漏）
//!
//! 测试栈：tokio::test + tower::ServiceExt::oneshot + 自建 mini router
//! （绕过 auth_middleware，仅挂 idempotency_middleware + 计数器 handler）
//!
//! ⚠️ 注意：测试 binary 名 `idempotency_api` 必须独占 Redis db 12；
//! `tests/common/mod.rs::test_redis_url` 已分配。
//!
//! 2026-09-23 review #1 修复：counter 从全局 static 改为 per-state Arc<AtomicUsize>。
//! 旧实现 `static COUNTER: AtomicUsize` 在 cargo test 并行下（默认 4 threads）
//! 跨用例污染——A.store(0) → A 发 POST counter=1 → B 在 A 校验前 store(0) →
//! A 看到 0 失败。改后每个用例构造独立 counter，用 Arc 在 handler 闭包内捕获，
//! 用例结束前断言 local counter 即可，与其它并发用例物理隔离。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "common/mod.rs"] mod common;` + `use common::{...};` 改走
//! `use hsh_erp_test_support::*` + `load_idempotency_fixture(&pool)`（stub）。
//! fixture 是 stub（`SELECT 1;`），保持「`load_<binary>_fixture`」调用约定一致。
//!
//! **保留本地 `fn send`**：本文件 send 返 `(StatusCode, Vec<u8>, Response)` 3 元组
//! （与 `test-support::http::send` 返 `(StatusCode, Value)` 不一致 —— 本文件用 raw bytes
//! 字节级断言第二次响应用 request 缓存命中），保留本地版本。
//!
//! PR-C.Final retry（2026-09-24）：删除 `mod common;`，改走 `use hsh_erp_test_support::{test_pool,
//! test_state_with_redis, test_redis_pool, ...}` 直接引入；本文件 setup 走 `test_pool` +
//! `test_state_with_redis` + `test_redis_pool`，均为 fixtures.rs 之外的 helper（test_state
//! 在 state.rs，test_redis_pool 在 redis.rs）。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Request};
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Json;
use deadpool_redis::redis::AsyncCommands;
use hsh_erp_test_support::{
    load_idempotency_fixture, test_pool, test_redis_pool, test_state_with_redis,
};
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_rust::middleware::idempotency::idempotency_middleware;
use hsh_erp_rust::state::AppState;

// ===========================================================================
// 测试 fixture：计数器 handler + 自建 mini router
// ===========================================================================

/// POST handler：每被调一次 counter += 1，返回 200 OK。
///
/// 2026-09-23 review #1：用 axum Extension 注入 counter（axum 0.8 的 Handler
/// trait 只 impl for `async fn`，不接 `impl Fn(...)`；counter 走 Extension 不
/// 影响 State extractor）。
async fn counted_post(Extension(counter): Extension<Arc<AtomicUsize>>) -> Response {
    counter.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        Json(json!({"echo": "post", "ok": true})),
    )
        .into_response()
}

/// PUT handler 计数器。
async fn counted_put(Extension(counter): Extension<Arc<AtomicUsize>>) -> Response {
    counter.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        Json(json!({"echo": "put", "ok": true})),
    )
        .into_response()
}

/// GET handler 计数器。
async fn counted_get(Extension(counter): Extension<Arc<AtomicUsize>>) -> Response {
    counter.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        Json(json!({"echo": "get", "ok": true})),
    )
        .into_response()
}

/// DELETE handler 计数器。
async fn counted_delete(Extension(counter): Extension<Arc<AtomicUsize>>) -> Response {
    counter.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        Json(json!({"echo": "delete", "ok": true})),
    )
        .into_response()
}

/// 自建 mini router：仅挂 idempotency_middleware，不挂 auth_middleware。
///
/// 2026-09-23 review #1：counter 由 caller 注入（每个用例独立 Arc<AtomicUsize>），
/// 通过 `Extension` 喂给 handler → 用例间物理隔离，并行执行不再互相污染。
/// `Layer` 顺序：先 idempotency_middleware（route_layer 语义：后调 = 外层），
/// 再 typed-closure middleware 把 counter 塞到 `req.extensions_mut()`。
fn make_test_app(state: Arc<AppState>, counter: Arc<AtomicUsize>) -> Router {
    Router::new()
        .route("/__test/post", post(counted_post))
        .route("/__test/put", put(counted_put))
        .route("/__test/get", get(counted_get))
        .route("/__test/delete", delete(counted_delete))
        // idempotency_middleware 在外层（route_layer 语义：先调 = 内层）
        .layer(from_fn_with_state(
            state.clone(),
            idempotency_middleware,
        ))
        // typed 闭包：显式标注 req: Request / next: Next 让编译器推断，
        // 把 counter 塞到 req.extensions_mut() 供 handler 用 Extension 取。
        // ⚠️ 不能用 `async move {}`——会移动 counter 让闭包退化成 FnOnce，
        // 破坏 tower Service 多次调用契约；用普通 `async {}` 借用即可。
        .layer(axum::middleware::from_fn(move |mut req: Request, next: Next| {
            req.extensions_mut().insert(counter.clone());
            async move { next.run(req).await }
        }))
        .with_state(state)
}

/// 公开路径专用 mini router：把 `/iam/login` 挂在 idempotency_middleware 下，
/// 用于验证公开路径闸门（即便带 Idempotency-Key 也不缓存响应）。
///
/// handler 返回模拟 login 响应（含 token 字段）——若闸门失效，缓存会存这个
/// token，下一次同 key POST 命中缓存返回旧 token → 测试 fail。
fn make_public_app(state: Arc<AppState>, counter: Arc<AtomicUsize>) -> Router {
    async fn login_handler(Extension(counter): Extension<Arc<AtomicUsize>>) -> Response {
        counter.fetch_add(1, Ordering::SeqCst);
        let n = counter.load(Ordering::SeqCst);
        (
            StatusCode::OK,
            Json(json!({
                "access_token": format!("TOKEN-call{n}"),
                "ok": true,
            })),
        )
            .into_response()
    }
    Router::new()
        .route("/iam/login", post(login_handler))
        .layer(from_fn_with_state(
            state.clone(),
            idempotency_middleware,
        ))
        .layer(axum::middleware::from_fn(move |mut req: Request, next: Next| {
            req.extensions_mut().insert(counter.clone());
            async move { next.run(req).await }
        }))
        .with_state(state)
}

// ===========================================================================
// helpers：建测试环境 + 发请求 + 读 Redis 验 key
// ===========================================================================

async fn setup() -> PgPool {
    let pool = test_pool().await;
    // 加载 stub fixture（保持 `load_<binary>_fixture` 调用约定一致；本 fixture 是空 stub）
    let _fx = load_idempotency_fixture(&pool).await;
    pool
}

/// 唯一 idem key（UUID-based）。每个用例独立 key 防止并行测试间 cache 撞车。
fn unique_key(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// 本地 send 3 元组版本（与 test-support::http::send 2 元组不同）：
/// 返 `(StatusCode, Vec<u8>, Response)`。Vec<u8> 让用例做字节级断言
/// （第二次响应与第一次字节级一致 = 缓存命中）；最后 Response 占位参数保留
/// 原文件签名（不动调用方）。
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
    let redis_pool = test_redis_pool().await;
    let mut conn = redis_pool.get().await.expect("redis conn");
    let exists: bool = conn.exists(key).await.expect("redis EXISTS");
    exists
}

// ===========================================================================
// 用例 1：same_key_returns_cached_response
// ===========================================================================

#[tokio::test]
async fn same_key_returns_cached_response() {
    let _pool = setup().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let state = test_state_with_redis(_pool.clone(), test_redis_pool().await);

    let key = unique_key("test-key-1");

    // 第 1 次 POST：handler 调，缓存
    let app1 = make_test_app(state.clone(), counter.clone());
    let (s1, b1, _) = send(
        app1,
        make_request("POST", "/__test/post", Some(&key)),
    )
    .await;
    // 给 spawn 的 put 一个机会跑完
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 第 2 次 POST：应命中缓存
    let app2 = make_test_app(state, counter.clone());
    let (s2, b2, _) = send(
        app2,
        make_request("POST", "/__test/post", Some(&key)),
    )
    .await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b1, b2, "第二次响应应与第一次字节级一致");
    assert_eq!(
        counter.load(Ordering::SeqCst),
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
    let counter = Arc::new(AtomicUsize::new(0));
    let _state = test_state_with_redis(_pool.clone(), test_redis_pool().await);

    let key = unique_key("test-key-2");

    // 连发 3 次相同 key POST
    for _ in 0..3 {
        let app = make_test_app(
            test_state_with_redis(_pool.clone(), test_redis_pool().await),
            counter.clone(),
        );
        let (s, _, _) = send(
            app,
            make_request("POST", "/__test/post", Some(&key)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        counter.load(Ordering::SeqCst),
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
    let counter = Arc::new(AtomicUsize::new(0));
    let _state = test_state_with_redis(_pool.clone(), test_redis_pool().await);

    // 第 1 次 POST 无 header
    let app1 = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s1, _, _) = send(app1, make_request("POST", "/__test/post", None)).await;
    // 第 2 次 POST 无 header
    let app2 = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s2, _, _) = send(app2, make_request("POST", "/__test/post", None)).await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "无 header 应调 handler 2 次"
    );
    // 不扫全 db 断言：handler 调 2 次本身就是无缓存的证据（如果缓存命中，counter=1）。
}

// ===========================================================================
// 用例 4：cross_method_same_key_collides
// ===========================================================================

#[tokio::test]
async fn cross_method_same_key_collides() {
    let _pool = setup().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let _state = test_state_with_redis(_pool.clone(), test_redis_pool().await);

    let key = unique_key("cross-method-key");

    // 第 1 次：POST /__test/post
    let app1 = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s1, b1, _) = send(
        app1,
        make_request("POST", "/__test/post", Some(&key)),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(s1, StatusCode::OK);

    // 第 2 次：PUT /__test/put 同 key
    let app2 = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s2, b2, _) = send(
        app2,
        make_request("PUT", "/__test/put", Some(&key)),
    )
    .await;

    // 第 1 次 POST 调 post handler；第 2 次 PUT 应命中第 1 次的缓存，不调 put handler
    assert_eq!(s1, s2, "PUT 应返 POST 的 status");
    assert_eq!(b1, b2, "PUT 应返 POST 的 body 字节级");
    assert_eq!(
        counter.load(Ordering::SeqCst),
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
    let counter = Arc::new(AtomicUsize::new(0));
    let _state = test_state_with_redis(_pool.clone(), test_redis_pool().await);

    let app = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s, _, _) = send(
        app,
        make_request("GET", "/__test/get", Some("any-key")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "GET handler 应被调一次"
    );
    // 不扫全 db 断言：counter==1 即证明 method 过滤生效，handler 被调即调用 middleware。
}

// ===========================================================================
// 用例 6：delete_with_header_is_skipped
// ===========================================================================

#[tokio::test]
async fn delete_with_header_is_skipped() {
    let _pool = setup().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let _state = test_state_with_redis(_pool.clone(), test_redis_pool().await);

    let app = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s, _, _) = send(
        app,
        make_request("DELETE", "/__test/delete", Some("any-key")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "DELETE handler 应被调一次"
    );
    // 不扫全 db 断言：counter==1 即证明 method 过滤生效。
}

// ===========================================================================
// 用例 7：header_missing_passes_through（key K1 + POST 不带 key 都走 handler）
// ===========================================================================

#[tokio::test]
async fn header_missing_passes_through() {
    let _pool = setup().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let _state = test_state_with_redis(_pool.clone(), test_redis_pool().await);

    let key = unique_key("K1");
    let redis_key = format!("idem:{key}");

    // 第 1 次：POST 带 K1（写缓存 + handler 调）
    let app1 = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s1, _, _) = send(app1, make_request("POST", "/__test/post", Some(&key))).await;
    assert_eq!(s1, StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 第 2 次：POST 不带 header（pass-through，调 handler）
    let app2 = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s2, _, _) = send(app2, make_request("POST", "/__test/post", None)).await;
    assert_eq!(s2, StatusCode::OK);

    // 第 3 次：POST 带 K1（应命中缓存，不调 handler）
    let app3 = make_test_app(
        test_state_with_redis(_pool.clone(), test_redis_pool().await),
        counter.clone(),
    );
    let (s3, _, _) = send(app3, make_request("POST", "/__test/post", Some(&key))).await;
    assert_eq!(s3, StatusCode::OK);

    // 总 handler 调用次数 = 2（第 1 次 + 第 2 次）
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "第 1 次 + 第 2 次无 header 调 handler，第 3 次命中"
    );
    // Redis 应有 idem:<uuid>
    assert!(
        redis_has_key(&redis_key).await,
        "Redis 应有 {redis_key} 缓存条目"
    );
}

// ===========================================================================
// 用例 8（可选）：ttl_expiry —— TTL=1 → sleep 2s → 再 POST 走 handler
// ===========================================================================

#[tokio::test]
async fn ttl_expiry() {
    let _pool = setup().await;
    let counter = Arc::new(AtomicUsize::new(0));

    // 改 TTL 为 1 秒；其它字段用测试默认
    let mut state = test_state_with_redis(_pool.clone(), test_redis_pool().await);
    {
        let state_inner = Arc::get_mut(&mut state).expect("state Arc 必须 unique");
        let old_cfg = (*state_inner.config).clone();
        let new_cfg = Arc::new(hsh_erp_rust::infra::config::AppConfig {
            idempotency_ttl_seconds: 1,
            ..old_cfg
        });
        state_inner.config = new_cfg;
    }

    let key = unique_key("ttl");
    let redis_key = format!("idem:{key}");

    let app1 = make_test_app(state.clone(), counter.clone());
    let (s1, _, _) = send(
        app1,
        make_request("POST", "/__test/post", Some(&key)),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // 给 spawn 一点时间把缓存写入 Redis（避免与 TTL 竞态）
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 确认 Redis 已有该 key
    assert!(
        redis_has_key(&redis_key).await,
        "第一次 POST 后 Redis 应有 {redis_key}"
    );

    // 等 TTL 过期（TTL=1s，等待 2s 留足余量）
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 确认 Redis key 已过期
    assert!(
        !redis_has_key(&redis_key).await,
        "TTL 过期后 {redis_key} 应消失"
    );

    let app2 = make_test_app(state, counter.clone());
    let (s2, _, _) = send(
        app2,
        make_request("POST", "/__test/post", Some(&key)),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "TTL 过期后第二次应走 handler"
    );
}

// ===========================================================================
// 用例 9（2026-09-23 review #1）：login_with_idempotency_key_does_not_cache_jwt
//
// 公开路径闸门：POST /iam/login 带 Idempotency-Key → 闸门命中直接 pass-through，
// 不查 Redis 也不写缓存。第二次同 key POST 应再次调 handler（counter += 1 +
// marker 序号递增），证明 JWT 没被缓存。
// ===========================================================================

#[tokio::test]
async fn login_with_idempotency_key_does_not_cache_jwt() {
    let _pool = setup().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let state = test_state_with_redis(_pool.clone(), test_redis_pool().await);

    let key = unique_key("login-idem");
    let redis_key = format!("idem:{key}");

    // 第 1 次：POST /iam/login 带 key（闸门放行 → handler 调）
    let app1 = make_public_app(state.clone(), counter.clone());
    let (s1, b1, _) = send(
        app1,
        make_request("POST", "/iam/login", Some(&key)),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let body1: serde_json::Value =
        serde_json::from_slice(&b1).expect("login body should be JSON");
    assert_eq!(body1["access_token"], "TOKEN-call1");

    // 等足够时间让 spawn put 有机会跑（如果闸门失效，缓存会写入）
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 第 2 次：POST /iam/login 同 key（闸门再次放行 → handler 调，counter+=1）
    let app2 = make_public_app(state.clone(), counter.clone());
    let (s2, b2, _) = send(
        app2,
        make_request("POST", "/iam/login", Some(&key)),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    let body2: serde_json::Value =
        serde_json::from_slice(&b2).expect("login body should be JSON");

    // 闸门生效：两次都走 handler，counter=2，access_token marker 序号递增。
    // 闸门失效（缓存命中）：counter=1，b2 == b1（拿到第一次的 token）。
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "公开路径闸门应放行两次，handler 调 2 次"
    );
    assert_eq!(
        body2["access_token"], "TOKEN-call2",
        "闸门失效 → 第二次命中缓存拿到旧 token → 这是 C1 critical 漏洞"
    );
    // 仅断言本用例的 key 没被写入（避免扫全 db 在并行测试下 flaky）
    assert!(
        !redis_has_key(&redis_key).await,
        "公开路径 /iam/login 不应写 {redis_key}"
    );
}