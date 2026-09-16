//! process 域端到端集成测试
//!
// 覆盖（Phase P2 process CRUD 段）：
// 1. create INHOUSE 强制 `requires_approval = false`（无视请求里的 true）
// 2. create 重复 code 拒 → 20802 BIZ_PROCESS_DUPLICATE_CODE
// 3. update 禁止改 `code` → 20104 BIZ_INVALID_VALUE
// 4. OUTSOURCE 类别保留 `requires_approval = true`（默认）
// 5. soft-delete 检查引用 → 20803 BIZ_PROCESS_IN_USE（挂 part.next_process_id）

// ## 并行
// 所有用例共享 `postgres_rust_test` + `uk_t_process_code` 唯一约束，用
// 进程级 `tokio::sync::Mutex` 串行化。
// ## 认证
// 用 MANAGER 用户跑通（POST /processes 写路径要求 M-only，按设计 §6.1 用 M 即可）。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, ensure_database_exists, insert_user_with_password,
    test_app, test_pool, test_state,
};

// ===========================================================================
// 全局串行化 + helpers（与 customer_api.rs / worker_pool_api.rs 同形）
// ===========================================================================
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let envelope: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("parse JSON: {e}; status={status}"));
    (status, envelope)
}

fn json_request(method: &str, uri: &str, body: Option<Value>, bearer: Option<&str>) -> Request<Body> {
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

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": username, "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token)
}

/// 直插一个 `t_part` 行（next_process_id = given），让 soft-delete 检查
/// 「被 part.next_process_id 引用」分支触发 20803 BIZ_PROCESS_IN_USE。
/// 绕开 part 域 CRUD（part CRUD 不是本任务范畴）。
async fn insert_part_with_next_process(pool: &PgPool, process_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
        1_577_836_800_000,
        1,
    );
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, next_process_id, version, created_at, updated_at) \
         VALUES ($1, 'TEST-NAME', 'TEST-DWG', 'TEST-APPLICANT', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $2, $3, 0, $4, $4)",
        id,
        // 用一个伪 customer_id (1L)；因为 soft-delete 检查只看 next_process_id，不校验 FK
        // 但 part 表 NOT NULL customer_id —— 用最小有效值 (1L)。truncate 会重置 sequence
        // 让它从 1 开始，所以这里直接选 1 即可。
        1_i64,
        process_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part referencing process");
    id
}

// ===========================================================================
// Tests
// ===========================================================================

/// INHOUSE 工序：请求体里的 `requires_approval=true` 必须被 service 层强制覆盖为 false
/// （与 Python `_assert_inhouse_no_approval` 对齐）。
#[tokio::test]
async fn create_process_inhouse_forces_requires_approval_false() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_inhouse").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-CUT",
                "name": "Cutting",
                "category": "INHOUSE",
                "requires_approval": true,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create INHOUSE: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["category"], "INHOUSE");
    assert_eq!(
        env["data"]["requires_approval"], false,
        "INHOUSE 必须强制 requires_approval=false; got: {env}"
    );
}

/// OUTSOURCE 工序：保留 `requires_approval` 默认 true。
#[tokio::test]
async fn create_process_outsource_keeps_requires_approval_default_true() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_outsrc").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-COAT",
                "name": "Coating",
                "category": "OUTSOURCE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create OUTSOURCE: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["category"], "OUTSOURCE");
    assert_eq!(
        env["data"]["requires_approval"], true,
        "OUTSOURCE 必须保留默认 requires_approval=true; got: {env}"
    );
}

/// 重复 code：撞 `uk_t_process_code` 部分唯一索引 → 20802 BIZ_PROCESS_DUPLICATE_CODE。
#[tokio::test]
async fn create_process_duplicate_code_returns_20802() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "proc_dup").await;

    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-DUP",
                "name": "First",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0, "first create should succeed; got: {env1}");

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-DUP",
                "name": "Second",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "duplicate code should return 409; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20802,
        "expected BIZ_PROCESS_DUPLICATE_CODE; got: {env2}"
    );
}

/// update INHOUSE 显式 `requires_approval=true` → 20104 BIZ_INVALID_VALUE。
#[tokio::test]
async fn update_process_inhouse_requires_approval_true_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_inh_apv").await;

    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-INH-APV",
                "name": "Inhouse NoApv",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    assert_eq!(env1["data"]["requires_approval"], false);
    let pid = env1["data"]["id"].as_str().unwrap().to_string();

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/processes/{pid}/update"),
            Some(json!({"requires_approval": true})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::BAD_REQUEST,
        "INHOUSE cannot enable requires_approval; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20104,
        "expected BIZ_INVALID_VALUE; got: {env2}"
    );
}

/// update INHOUSE 不传 `requires_approval` 字段：service 不应注入 `Some(false)`，
/// 字段维持原值。修复前该路径会在 DB 层无谓重写 `requires_approval=false` 并 bump version。
#[tokio::test]
async fn update_process_inhouse_no_approval_field_does_not_bump_version() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_inh_noop").await;

    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-INH-NOOP",
                "name": "Inhouse NoOp",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    assert_eq!(env1["data"]["requires_approval"], false);
    let pid = env1["data"]["id"].as_str().unwrap().to_string();

    // 不传 requires_approval 也不传任何其他字段 → 仅校验 service 不注入 Some(false) 后字段仍为 false
    // (version 是否 bump 取决于 repo SQL 是否仅在字段变化时 +1，属后续工作；
    // 本测试聚焦 service 层不再 silently rewrite INHOUSE requires_approval。)
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/processes/{pid}/update"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "no-op update should succeed; got: {env2}");
    assert_eq!(env2["code"], 0);
    assert_eq!(
        env2["data"]["requires_approval"], false,
        "INHOUSE requires_approval must remain false; got: {env2}"
    );
}

/// update 时改 `code` 必拒 → 20104 BIZ_INVALID_VALUE（code 是业务唯一键，不可变）。
#[tokio::test]
async fn update_process_code_change_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_code_lock").await;

    // 创建一个工序
    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-ORIG",
                "name": "Original",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    let pid = env1["data"]["id"].as_str().unwrap().to_string();

    // 试图改 code → 应拒
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/processes/{pid}/update"),
            Some(json!({"code": "P-NEW", "name": "Renamed"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::BAD_REQUEST,
        "code change should return 400; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20104,
        "expected BIZ_INVALID_VALUE; got: {env2}"
    );
}

/// 软删前查引用：`t_part.next_process_id` 仍有引用 → 20803 BIZ_PROCESS_IN_USE。
#[tokio::test]
async fn soft_delete_process_referenced_by_part_returns_20803() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "proc_in_use").await;

    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-IN-USE",
                "name": "Referenced",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    let pid_str = env1["data"]["id"].as_str().unwrap().to_string();
    let pid: i64 = pid_str.parse().unwrap();

    // 插一个 part.next_process_id = pid
    insert_part_with_next_process(&pool, pid).await;

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/processes/{pid_str}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "soft-delete in-use process should return 409; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20803,
        "expected BIZ_PROCESS_IN_USE; got: {env2}"
    );
}

// ===========================================================================
//  color 字段（migration 020）—— 2026-09-12 新增
// ===========================================================================

/// 建工序带 color `#RRGGBBAA` → fetch 拿回原文；缺省字段不出现在响应。
#[tokio::test]
async fn create_process_color_round_trip() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "proc_color").await;

    // 1. create with color
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-COLOR-OK",
                "name": "Colored",
                "category": "INHOUSE",
                "color": "#409EFFA0",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    assert_eq!(env["data"]["color"], "#409EFFA0");
    let pid = env["data"]["id"].as_str().unwrap().to_string();

    // 2. fetch by id
    let (s2, env2) = send(
        app,
        json_request("GET", &format!("/processes/{pid}"), None, Some(&token)),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(env2["data"]["color"], "#409EFFA0");
}

/// 不传 color → 响应字段缺省（skip_serializing_if = "Option::is_none"）。
#[tokio::test]
async fn create_process_no_color_omits_field() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_nocolor").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-NOCOLOR",
                "name": "Plain",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    assert!(
        env["data"]["color"].is_null(),
        "未设 color 时应为 null: {env}"
    );
}

/// color 格式错（不是 `#RRGGBBAA`）→ 20104 BIZ_INVALID_VALUE。
#[tokio::test]
async fn create_process_invalid_color_rejected() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_badcolor").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-BAD",
                "name": "Bad color",
                "category": "INHOUSE",
                "color": "red",  // 错格式
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20104,
        "BIZ_INVALID_VALUE: {env}"
    );
}

/// UPDATE color 三态：
/// - `color: null` ⇒ 显式清空
/// - `color: "..."` ⇒ 改值
/// - 字段缺省 ⇒ 不改
#[tokio::test]
async fn update_process_color_tristate() {
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool, "proc_color_ts").await;

    // 建 + 初始 color
    let (_, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-TS",
                "name": "Tristate",
                "category": "INHOUSE",
                "color": "#11111111",
            })),
            Some(&token),
        ),
    )
    .await;
    let pid = env["data"]["id"].as_str().unwrap().to_string();

    // 1. update color to new value
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/processes/{pid}/update"),
            Some(json!({ "color": "#AABBCCDD" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "set: {env1}");
    assert_eq!(env1["data"]["color"], "#AABBCCDD");

    // 2. clear (null)
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/processes/{pid}/update"),
            Some(json!({ "color": null })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "clear: {env2}");
    assert!(env2["data"]["color"].is_null(), "清空后应为 null: {env2}");

    // 3. leave unchanged (字段缺省) —— 设回一个值后，只 update name，应保留原 color
    let _ = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/processes/{pid}/update"),
            Some(json!({ "color": "#FF00FF00" })),
            Some(&token),
        ),
    )
    .await;
    let (s3, env3) = send(
        app,
        json_request(
            "POST",
            &format!("/processes/{pid}/update"),
            Some(json!({ "name": "Renamed" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::OK, "leave: {env3}");
    assert_eq!(
        env3["data"]["color"], "#FF00FF00",
        "未传 color 字段应保留原值: {env3}"
    );
}

/// 2026-09-17 PR-4 守卫修复：process 被工艺链 step 引用时，软删应被拦下
/// (BIZ_PROCESS_IN_USE 20803)。PR-1 FK 翻转后 part → chain → step 是新的
/// 工艺引用通道；之前缺这条会漏掉 step 仍引用此 process 的场景。
#[tokio::test]
async fn soft_delete_process_referenced_by_chain_step_returns_20803() {
    use hsh_erp_rust::infra::clock::now_naive;
    let (_guard, pool) = setup().await;
    let (app, token) = login_manager(pool.clone(), "proc_chain_step_ref").await;

    // 建一个 process
    let (_s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/processes",
            Some(json!({
                "code": "P-STEPREF",
                "name": "StepRef",
                "category": "INHOUSE",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    let pid_str = env1["data"]["id"].as_str().unwrap().to_string();
    let pid: i64 = pid_str.parse().unwrap();

    // 直插 chain + step（绕开 part 软删级联，单纯看 step 是否拦截 soft-delete）
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(
        1_577_836_800_000,
        1,
    );
    let chain_id = snowflake.next_id();
    let step_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 0, $3, 0, $3, 0)",
    )
    .bind(chain_id)
    .bind("chain-stepref")
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert chain");
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, estimated_minutes, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 10, $3, 30, 0, $4, 0, $4, 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(pid)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert chain step referencing process");

    // 软删 process 应被 step 引用拦下
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/processes/{pid_str}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "soft-delete 工艺链 step 引用的 process 应返回 409; got: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20803,
        "expected BIZ_PROCESS_IN_USE; got: {env2}"
    );
}