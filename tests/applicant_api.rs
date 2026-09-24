//! applicant 域 6 端点集成测试（Task 5 / plan §11）
//!
//! 覆盖：
//!   1. list_applicants_empty                          — 空 DB GET /com/applicants → items=[]
//!   2. create_get_update_soft_delete_applicant_happy_path — 全生命周期 + version 递增 + 软删后 404
//!   3. create_with_l2_customer_returns_21003          — L1 校验
//!   4. duplicate_name_under_same_customer_returns_21002 — 同客户下重名
//!   5. update_with_stale_version_returns_409          — 乐观锁（并发事务让 UPDATE 撞 0 行）
//!   6. soft_delete_in_use_returns_21004               — 被 t_part.applicant_name 引用 → 拒软删
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//! 每个用例 MANAGER token；与其它 applicant 域用例共享同一 token 来源（用户独立）。
//!
//! ## URL 约定
//! `test_app` 返回 `v2_router()` 直挂，无 `/api/v2` 前缀 —— 故 URL 写 `/com/applicants` 而非
//! `/api/v2/com/applicants`（与 main.rs 的 `/api/v2` nest 区分；2026-09-19 applicant 聚合至 com nest）。
//! 该写法与 worker_pool_api.rs /
//! part_api.rs / delivery_*_api.rs 等保持一致。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "common/mod.rs"] mod common;` + `use common::{...};` 改走
//! `use hsh_erp_test_support::*` + `load_applicant_fixture(&pool)` +
//! `ApplicantFixture` + `bootstrap_as_manager` 样板。fixture 提供 1 MANAGER user
//! + 2 customer（L1+L2）+ 1 baseline applicant + 1 part 行（in-use 校验）。
//! 测试内的 local helpers（insert_l1 / insert_l2 / insert_part_referencing_applicant）
//! 仍走 snowflake ID 现场创建特定字面数据（用于 create / L2 校验 / 重名场景）。
//! 字面请求 / 断言逐字保留。

use sqlx::PgPool;

use hsh_erp_test_support::{
    ApplicantFixture, json_request, load_applicant_fixture, send as ts_send, test_app, test_pool,
    test_state,
};

// ===========================================================================
//  Helpers
// ===========================================================================

/// 走 test-support::http::send，alias 复用避免重复实现（PR13 Phase F 收敛）。
async fn send(
    app: axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, serde_json::Value) {
    ts_send(app, req).await
}

/// 重新构造 Router（oneshot 消耗 Router 之后）。
async fn fresh_app(pool: &PgPool) -> axum::Router {
    test_app(test_state(pool.clone()).await)
}

/// 基础 bootstrap + MANAGER 登录拿 token + 返回 (pool, token, fx)。
///
/// 登录本身消耗一个 Router，但用例后续用 `fresh_app(&pool)` 重建 Router 即可。
async fn bootstrap_as_manager() -> (PgPool, String, ApplicantFixture) {
    let pool = test_pool().await;
    let fx = load_applicant_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(serde_json::json!({"username": ApplicantFixture::MANAGER_USERNAME, "password": ApplicantFixture::PASSWORD})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    (pool, token, fx)
}

/// 基础 bootstrap 两个独立 MANAGER user（OCC 并发测试用）。
///
/// fixture 仅 1 MANAGER user，并发测试需 2 个独立 user 各持 token；
/// 在 fixture 提供的 fx_applicant_manager 基础上另建 1 个临时 MANAGER user 登录。
async fn bootstrap_as_manager_with_pair() -> (PgPool, String, String) {
    use hsh_erp_rust::auth::password;
    let pool = test_pool().await;
    let _fx = load_applicant_fixture(&pool).await;

    // 插第二个 MANAGER user（避免与 fixture 内 baseline 撞 username）
    let uid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES (DEFAULT, 'fx_applicant_mgr_b', $1, 'FX Applicant Mgr B', true, 0, 0, \
                 now(), now()) RETURNING id",
    )
    .bind(password::hash(ApplicantFixture::PASSWORD).expect("hash"))
    .fetch_one(&pool)
    .await
    .expect("insert second MANAGER user");
    sqlx::query(
        "INSERT INTO t_user_role (user_id, role, scope_type, scope_id, version, created_at, updated_at) \
         VALUES ($1, 'MANAGER', NULL, NULL, 0, now(), now())",
    )
    .bind(uid)
    .execute(&pool)
    .await
    .expect("add MANAGER role");

    // 登录两个 user 各拿 token
    let app_a = test_app(test_state(pool.clone()).await);
    let (_, env_a) = send(
        app_a,
        json_request(
            "POST",
            "/iam/login",
            Some(serde_json::json!({"username": ApplicantFixture::MANAGER_USERNAME, "password": ApplicantFixture::PASSWORD})),
            None,
        ),
    )
    .await;
    let token_a = env_a["data"]["token"].as_str().unwrap().to_string();

    let app_b = test_app(test_state(pool.clone()).await);
    let (_, env_b) = send(
        app_b,
        json_request(
            "POST",
            "/iam/login",
            Some(serde_json::json!({"username": "fx_applicant_mgr_b", "password": ApplicantFixture::PASSWORD})),
            None,
        ),
    )
    .await;
    let token_b = env_b["data"]["token"].as_str().unwrap().to_string();

    (pool, token_a, token_b)
}

// ----- Fixture helpers（applicant 域专用）-----

/// 插一个 L1 客户（parent_id IS NULL）；serial_prefix 取 `name` 首字母大写。
///
/// fixture 已预置 1 个 L1 customer（fx_applicant_l1），本 helper 给「create 测试需
/// 不同 L1」用例现场造新 L1（snowflake ID 避免 uk_t_customer_root_prefix 撞）。
async fn insert_l1(pool: &PgPool, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    // serial_prefix 是 varchar(1) + regex ^[A-Z]$ —— 取 name 首字母大写；fallback 'X'
    let one_char: String = name
        .chars()
        .next()
        .unwrap_or('X')
        .to_ascii_uppercase()
        .to_string();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id,
        name,
        one_char,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1 customer");
    id
}

/// 插一个 L2 客户（parent_id = l1_id）；serial_prefix 留 NULL（叶子节点无前缀）。
#[allow(dead_code)]
async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id,
        name,
        l1_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L2 customer");
    id
}

/// 插一条引用指定 `(applicant_name, customer_id)` 的 t_part（未软删）。
/// service 层 `count_parts_using_applicant_name` 据此判定 in-use。
/// 返回 part.id。
async fn insert_part_referencing_applicant(
    pool: &PgPool,
    customer_id: i64,
    applicant_name: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query!(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'D-PART', $3, $4, $5, $5, 'PENDING', 0, $6, NULL, $6, NULL)",
        id,
        format!("part-for-{applicant_name}"),
        applicant_name,
        customer_id,
        today,
        now,
    )
    .execute(pool)
    .await
    .expect("insert referencing t_part");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. 空 DB：GET /com/applicants → 200 / items=[] / total=0 / limit=100（DEFAULT_LIMIT）。
///
/// fixture 预置 sample_applicant + part_ref 行；为保留字面断言 `total=0`，
/// 在加载 fixture 后删掉 baseline applicant + 引用 part。
/// （保留字面断言优先于复用 fixture。）
#[tokio::test]
async fn list_applicants_empty() {
    let pool = test_pool().await;
    let fx = load_applicant_fixture(&pool).await;
    // 清掉 fixture 预置的 sample_applicant + part_ref（字面断言 total=0 优先）
    sqlx::query("DELETE FROM t_part WHERE id = $1")
        .bind(fx.part_ref_id)
        .execute(&pool)
        .await
        .expect("delete part_ref");
    sqlx::query("DELETE FROM t_applicant WHERE id = $1")
        .bind(fx.sample_applicant_id)
        .execute(&pool)
        .await
        .expect("delete sample_applicant");

    let (_, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/iam/login",
            Some(serde_json::json!({"username": ApplicantFixture::MANAGER_USERNAME, "password": ApplicantFixture::PASSWORD})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();

    let (s, env) = send(
        fresh_app(&pool).await,
        json_request("GET", "/com/applicants", None, Some(&token)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "list empty: {env}");
    assert_eq!(env["code"], 0);
    assert!(env["data"]["items"].is_array());
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 0);
    assert_eq!(env["data"]["total"], 0);
    assert_eq!(env["data"]["limit"], 100, "DEFAULT_LIMIT 应=100: {env}");
    assert_eq!(env["data"]["offset"], 0);
}

/// 2. 全生命周期 happy path：create → get → update → soft-delete → get 404。
/// 校验：
/// - create 后 version=0 / customer_name 已 join
/// - update 后 name 改变 + version=1（OCC 自增）
/// - soft-delete 后 GET 返 404 / code=21001 BIZ_APPLICANT_NOT_FOUND
#[tokio::test]
async fn create_get_update_soft_delete_applicant_happy_path() {
    let (pool, token, fx) = bootstrap_as_manager().await;

    // create
    let (s, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/applicants",
            Some(serde_json::json!({"name": "张三", "customer_id": fx.l1_customer_id.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create: {env}");
    assert_eq!(env["code"], 0);
    let id = env["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(env["data"]["name"], "张三");
    assert_eq!(env["data"]["customer_id"], fx.l1_customer_id.to_string());
    assert_eq!(env["data"]["customer_name"], "FX Applicant L1");
    assert_eq!(env["data"]["version"], 0);

    // get
    let (s, env) = send(
        fresh_app(&pool).await,
        json_request("GET", &format!("/com/applicants/{id}"), None, Some(&token)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "get: {env}");
    assert_eq!(env["data"]["id"], id);
    assert_eq!(env["data"]["name"], "张三");
    assert_eq!(env["data"]["customer_name"], "FX Applicant L1");

    // update
    let (s, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/com/applicants/{id}/update"),
            Some(serde_json::json!({"name": "李四"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "update: {env}");
    assert_eq!(env["data"]["name"], "李四");
    assert_eq!(env["data"]["version"], 1, "update 应 OCC +1: {env}");

    // soft-delete
    let (s, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/com/applicants/{id}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "soft-delete: {env}");
    assert_eq!(env["code"], 0);

    // 软删后 GET → 404 / code=21001
    let (s, env) = send(
        fresh_app(&pool).await,
        json_request("GET", &format!("/com/applicants/{id}"), None, Some(&token)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::NOT_FOUND, "软删后 GET 应 404: {env}");
    assert_eq!(env["code"], 21001, "BIZ_APPLICANT_NOT_FOUND: {env}");
}

/// 3. customer_id 指向 L2（非一级） → POST → 400 / 21003 BIZ_APPLICANT_BAD_CUSTOMER。
#[tokio::test]
async fn create_with_l2_customer_returns_21003() {
    let (pool, token, fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/applicants",
            Some(serde_json::json!({"name": "王五", "customer_id": fx.l2_customer_id.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST, "L2 应 400: {env}");
    assert_eq!(env["code"], 21003, "BIZ_APPLICANT_BAD_CUSTOMER: {env}");
}

/// 4. 同一 L1 下姓名重复 → POST → 409 / 21002 BIZ_APPLICANT_DUPLICATE_NAME。
#[tokio::test]
async fn duplicate_name_under_same_customer_returns_21002() {
    let (pool, token, fx) = bootstrap_as_manager().await;

    // 第一次创建同名成功
    let (s1, env1) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/applicants",
            Some(serde_json::json!({"name": "重复名", "customer_id": fx.l1_customer_id.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, axum::http::StatusCode::CREATED, "第一次 create: {env1}");

    // 第二次同名 → 409 / 21002
    let (s2, env2) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/applicants",
            Some(serde_json::json!({"name": "重复名", "customer_id": fx.l1_customer_id.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, axum::http::StatusCode::CONFLICT, "重名应 409: {env2}");
    assert_eq!(env2["code"], 21002, "BIZ_APPLICANT_DUPLICATE_NAME: {env2}");
}

/// 5. 乐观锁：并发 update 同一行，两个事务同时 SELECT V=0 → 两个都准备
///    UPDATE WHERE V=0 → 第二个 COMMIT 时 UPDATE 命中 0 行 → 40901 VERSION_CONFLICT。
///
/// 设计：tokio::join! 同时跑两个独立 Router（共享 Arc<AppState> + PgPool）的 update；
/// 由于 service 的"SELECT V THEN UPDATE WHERE V"两步在同一事务内，
/// READ COMMITTED 下两个并发事务会出现：一个先 UPDATE 提交 → 另一个 UPDATE 撞 0 行。
///
/// 断言：恰好 1 个 200 + 1 个 409。
#[tokio::test]
async fn update_with_stale_version_returns_409() {
    let (pool, token_a, token_b) = bootstrap_as_manager_with_pair().await;
    let pool_for_l1 = pool.clone();
    let l1_id = insert_l1(&pool_for_l1, "OccCo").await;
    drop(pool_for_l1);

    // 建一个 applicant V=0
    let (s, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/applicants",
            Some(serde_json::json!({"name": "occ-target", "customer_id": l1_id.to_string()})),
            Some(&token_a),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create: {env}");
    let id = env["data"]["id"].as_str().unwrap().to_string();

    // 两个并发 update（不同名字）—— 一个会成功 V=0→1，另一个会撞 stale version → 40901
    let uri = format!("/com/applicants/{id}/update");
    let req_a = json_request("POST", &uri, Some(serde_json::json!({"name": "name-A"})), Some(&token_a));
    let req_b = json_request("POST", &uri, Some(serde_json::json!({"name": "name-B"})), Some(&token_b));
    let app_a = fresh_app(&pool).await;
    let app_b = fresh_app(&pool).await;
    let (r1, r2) = tokio::join!(send(app_a, req_a), send(app_b, req_b));
    let (s_a, e_a) = r1;
    let (s_b, e_b) = r2;

    // 期望：恰好一个 200，一个 409
    let pair = [(s_a, &e_a), (s_b, &e_b)];
    let ok_count = pair.iter().filter(|(s, _)| *s == axum::http::StatusCode::OK).count();
    let conflict_count = pair
        .iter()
        .filter(|(s, _)| *s == axum::http::StatusCode::CONFLICT)
        .count();
    assert_eq!(
        ok_count, 1,
        "exactly one update should succeed: A={s_a}/{e_a}; B={s_b}/{e_b}"
    );
    assert_eq!(
        conflict_count, 1,
        "exactly one update should hit OCC 409: A={s_a}/{e_a}; B={s_b}/{e_b}"
    );
    let conflict_env = pair
        .iter()
        .find(|(s, _)| *s == axum::http::StatusCode::CONFLICT)
        .map(|(_, e)| e)
        .expect("find conflict response");
    assert_eq!(
        conflict_env["code"], 40901,
        "VERSION_CONFLICT: A={s_a}/{e_a}; B={s_b}/{e_b}"
    );
}

/// 6. in-use 校验：先建 applicant → 插 t_part 引用此 applicant_name + customer_id →
///    soft-delete → 409 / 21004 BIZ_APPLICANT_IN_USE。
#[tokio::test]
async fn soft_delete_in_use_returns_21004() {
    let (pool, token, fx) = bootstrap_as_manager().await;

    // 建 applicant
    let (s, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/applicants",
            Some(serde_json::json!({"name": "被引用名", "customer_id": fx.l1_customer_id.to_string()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create: {env}");
    let id = env["data"]["id"].as_str().unwrap().to_string();

    // 插 t_part 引用此 applicant_name + L1 customer_id
    let _ = insert_part_referencing_applicant(&pool, fx.l1_customer_id, "被引用名").await;

    // soft-delete → 409 / 21004
    let (s, env) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/com/applicants/{id}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::CONFLICT, "被引用应 409: {env}");
    assert_eq!(env["code"], 21004, "BIZ_APPLICANT_IN_USE: {env}");

    // 校验 applicant 仍存在（未被软删）
    let still_there: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!" FROM t_applicant WHERE id = $1 AND deleted_at IS NULL"#,
        id.parse::<i64>().unwrap(),
    )
    .fetch_one(&pool)
    .await
    .expect("count applicant");
    assert_eq!(still_there, 1, "in-use 时 applicant 应未被软删");
}