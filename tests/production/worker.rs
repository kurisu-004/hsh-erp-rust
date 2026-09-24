//! worker 域端到端集成测试
//!
//! ## 覆盖（Task 4 worker CRUD + verify-badge）
//! 1. `verify_badge_inactive_returns_20202` — 工人存在但 `is_active=false` →
//!    `verify-badge` 端点返回 400 + 20202 `BIZ_WORKER_INACTIVE`。
//! 2. `reactivate_worker_version_conflict_returns_40901` — 服务外偷偷改
//!    `t_worker.version`，reactivate 端点 → 409 + 40901 `VERSION_CONFLICT`
//!    （Task 4 修复：与「已激活无变化」场景分流）。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（写路径要求 M-only，按设计 §6.1 用 M 即可）。
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! `login_manager` 通用 helper，统一走 `use hsh_erp_test_support::{...}` +
//! `bootstrap_as_manager()` + `load_production_fixture(&pool)`。无独有 helper
//! （worker 域走 service seed API 创建，测试体本身已无 INSERT）。

use axum::http::StatusCode;
use serde_json::json;

use hsh_erp_test_support::{
    ProductionFixture, json_request, load_production_fixture, login_token, send, test_app,
    test_pool, test_state,
};

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 production fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。
async fn bootstrap_as_manager() -> (sqlx::PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn verify_badge_inactive_returns_20202() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    // Create worker (active by default).
    let (s_create, env_create) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/workers",
            Some(json!({"badge_code": "B001", "name": "Alice"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_create, StatusCode::CREATED, "create worker: {env_create}");
    assert_eq!(env_create["code"], 0);
    let wid = env_create["data"]["id"].as_str().unwrap().to_string();

    // Deactivate (sets is_active=false + deleted_at=now()).
    let (s_deact, env_deact) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/workers/{wid}/deactivate"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_deact, StatusCode::OK, "deactivate worker: {env_deact}");

    // verify-badge → 20202 (BIZ_WORKER_INACTIVE) with HTTP 400
    let (s_verify, env_verify) = send(
        app,
        json_request(
            "POST",
            "/prod/workers/verify-badge",
            Some(json!({"badge_code": "B001"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s_verify,
        StatusCode::BAD_REQUEST,
        "verify-badge on inactive worker should return 400; got {env_verify}"
    );
    assert_eq!(
        env_verify["code"].as_i64().unwrap(),
        20202,
        "expected BIZ_WORKER_INACTIVE; got envelope: {env_verify}"
    );
}

/// Task 4 修复回归：service 在 `reactivate` 返回 0 行时，必须区分
/// 「版本冲突」(40901) 与 「已激活无变化」(BIZ_INVALID_VALUE) 两个语义。
///
/// 真实 OCC 需要并发 update（service re-read 会拿到最新 version，单连接内
/// 没法构造），本测试用 raw SQL 把行构造成「`is_active=false, deleted_at=NULL`
/// 的状态机不一致」：service 读到 `is_active=false` ⇒ 进入 OCC 分支；UPDATE 因
/// `WHERE deleted_at IS NOT NULL` 不匹配 ⇒ `affected=0` ⇒ 命中 VERSION_CONFLICT。
/// 这个分支在正常流程（deactivate 一定同时写 `deleted_at=now()`）下不可达，但
/// 是 service 的正确分流必须能处理它。
#[tokio::test]
async fn reactivate_worker_version_conflict_returns_40901() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 1) 建一个 active 工人
    let (s_create, env_create) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/workers",
            Some(json!({"badge_code": "B-VC-001", "name": "Bob"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_create, StatusCode::CREATED, "create worker: {env_create}");
    let wid_str = env_create["data"]["id"].as_str().unwrap().to_string();
    let wid: i64 = wid_str.parse().unwrap();

    // 2) 直接 raw SQL 把行构造成「is_active=false, deleted_at=NULL」（人工不一致状态）：
    //    - service re-read 拿到 is_active=false ⇒ 不进「已激活」分支
    //    - service 的 UPDATE `WHERE deleted_at IS NOT NULL` ⇒ 0 行 ⇒ 走 VERSION_CONFLICT
    sqlx::query("UPDATE t_worker SET is_active = false WHERE id = $1")
        .bind(wid)
        .execute(&pool)
        .await
        .expect("force is_active=false state");

    // 3) reactivate ⇒ 期望 409 + 40901
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/workers/{wid_str}/reactivate"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "expected 409 on OCC branch; got {env}"
    );
    assert_eq!(
        env["code"].as_i64().unwrap(),
        40901,
        "expected VERSION_CONFLICT; got envelope: {env}"
    );
}