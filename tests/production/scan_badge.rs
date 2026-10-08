//! prod::scan 扫工牌端点（`POST /api/v2/prod/scan/verify-badge`）集成测试
//!
//! 2026-10-10 自 `tests/production/worker.rs` 迁来（端点随 `verify-badge` 一起从
//! `prod::worker` 硬切到 `prod::scan`，**无 alias**）。
//!
//! ## 覆盖
//! 1. `verify_badge_inactive_returns_20202` — 工人存在但 `is_active=false` →
//!    HTTP 400 + 20202 `BIZ_WORKER_INACTIVE`。
//! 2. `verify_badge_unknown_returns_20201` — 工牌不存在 → HTTP 404 + 20201。
//! 3. `verify_badge_response_has_exactly_four_keys` — ★ 出参形状：`ScanWorkerBrief`
//!    只有 4 个键（报工台三页合计只读 `id` / `badge_code` / `name` / `work_type_id`）。
//! 4. `verify_badge_is_open_to_shelf_account` — 任意已登录用户可调（含
//!    SHELF_ACCOUNT），不设角色白名单。
//! 5. `old_verify_badge_path_is_gone` — 旧路径 `POST /prod/workers/verify-badge`
//!    已失效。
//!
//! ## 认证
//! 场景 1/2/3/5 用 MANAGER；场景 4 用 SHELF_ACCOUNT（本端点存在的理由就是工人
//! 自己用工牌开机）。
//!
//! ## 串行化
//! 进程级 test_pool 每次 fresh database，DB 间 schema 完全独立，无需串行化。

use axum::http::StatusCode;
use serde_json::{Value, json};

use hsh_erp_test_support::{
    ProductionFixture, json_request, load_production_fixture, login_token, send, send_raw,
    test_app, test_pool, test_state,
};

/// 起一份 fresh database + 加载 production fixture + 以 MANAGER 身份登录。
async fn bootstrap_as_manager() -> (sqlx::PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 造一个 active 工人，返回 `(worker_id, badge_code)`。
async fn create_worker(
    app: &axum::Router,
    token: &str,
    badge: &str,
    work_type_id: Option<i64>,
) -> String {
    let mut payload = json!({"badge_code": badge, "name": "Alice"});
    if let Some(wt) = work_type_id {
        payload["work_type_id"] = json!(wt.to_string());
    }
    let (status, env) = send(
        app.clone(),
        json_request("POST", "/prod/workers", Some(payload), Some(token)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create worker: {env}");
    env["data"]["id"]
        .as_str()
        .expect("worker id 是 JSON string")
        .to_string()
}

#[tokio::test]
async fn verify_badge_inactive_returns_20202() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let wid = create_worker(&app, &token, "B001", None).await;

    // deactivate（写 `is_active=false` + `deleted_at=now()`）
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
            "/prod/scan/verify-badge",
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

/// 工牌未注册 → 20201 `BIZ_WORKER_NOT_FOUND`（HTTP 404）。
///
/// 这条与 20202 的分流靠 `include_deleted=true` 实现：查得到但停用是 20202，
/// 查不到是 20201 —— 两个语义前端要显示的话完全不同。
#[tokio::test]
async fn verify_badge_unknown_returns_20201() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (status, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/scan/verify-badge",
            Some(json!({"badge_code": "NOPE-404"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "未知工牌应是 404: {env}");
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20201,
        "expected BIZ_WORKER_NOT_FOUND; got envelope: {env}"
    );
}

/// ★ 出参形状：`ScanWorkerBrief` 只有 4 个键。
///
/// 报工台四页合计只读 `id`（发 held 列表）/ `work_type_id`（发 pickable 列表）/
/// `badge_code`（顶栏 + worker-scan 入参）/ `name`（顶栏）。后端多给 8 个
/// `WorkerOut` 字段不会让任何 UI 多显示一行，却会让 Zod 逐个声明 8 个不会变的
/// 键；后端**少给**一个则顶栏直接空。这条断言把键集合钉死。
#[tokio::test]
async fn verify_badge_response_has_exactly_four_keys() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let _ = create_worker(&app, &token, "B-KEYS", Some(fx.work_type_a_id)).await;

    let (status, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/scan/verify-badge",
            Some(json!({"badge_code": "B-KEYS"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "扫活跃工牌应 200: {env}");
    assert_eq!(env["code"], 0, "{env}");

    let data = env["data"].as_object().expect("data 是对象");
    let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["badge_code", "id", "name", "work_type_id"],
        "ScanWorkerBrief 的字段集变了：加字段要同步前端 Worker 类型与 views/scan 的消费方，\
         删字段要先确认无消费方"
    );
    // 赋了工种的工人：work_type_id 是雪花 id 的 JSON string 形态
    let expect_wt = fx.work_type_a_id.to_string();
    assert_eq!(
        data["work_type_id"].as_str(),
        Some(expect_wt.as_str()),
        "work_type_id 必须是 JSON string: {env}"
    );
}

/// 任意已登录用户可调（含 SHELF_ACCOUNT）—— 本端点存在的理由就是工人自己开机。
///
/// 若哪天加了角色白名单，工人那一侧会在真机上直接打不开报工台，故显式钉住。
#[tokio::test]
async fn verify_badge_is_open_to_shelf_account() {
    let (_pool, app, mgr_token, fx) = bootstrap_as_manager().await;
    // 先用 MANAGER 造一个活跃工人（fixture 里没有预置的固定工牌号）
    let wid = create_worker(&app, &mgr_token, "B-SHELF", None).await;
    assert!(!wid.is_empty());

    let shelf_token = login_token(
        &app,
        &fx.part_shelf_account_username,
        ProductionFixture::PASSWORD,
    )
    .await;
    let (status, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/verify-badge",
            Some(json!({"badge_code": "B-SHELF"})),
            Some(&shelf_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "SHELF_ACCOUNT 必须能扫工牌（2026-10-10 契约：任意已登录用户可调）: {env}"
    );
    assert_eq!(env["data"]["badge_code"], "B-SHELF", "{env}");
}

/// 旧路径失效（硬切无 alias）。
///
/// ⚠️ `/prod/workers/verify-badge` 是 2 段 path，落进 worker 域的 `/{id}` —— 但那条
/// **只注册了 GET**（worker 详情是读端点）⇒ 方法不匹配，返 **405
/// METHOD_NOT_ALLOWED**。既不是 400（`Path` 提取器根本没机会拒绝）也不是 404。
/// 实际响应码按实跑结果写在这里，不要凭「路由删了就是 404」想当然。
#[tokio::test]
async fn old_verify_badge_path_is_gone() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    // 走 `send_raw`：405 的响应体是纯文本、不走 `R<T>` 信封，`send` 会在 JSON
    // 解析处 panic —— 而那正是本用例要断言的形态。
    let (status, raw) = send_raw(
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
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "旧路径应失效（落进 /prod/workers/{{id}}，那条只注册 GET ⇒ 405）: {raw}"
    );
}

/// 未登录 → 401（鉴权由 `CurrentUser` extractor 承担，service 层不重复校验）。
#[tokio::test]
async fn verify_badge_requires_login() {
    let (_pool, app, _token, _fx) = bootstrap_as_manager().await;
    let (status, _env): (StatusCode, Value) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/verify-badge",
            Some(json!({"badge_code": "B001"})),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "未登录必须是 401");
}
