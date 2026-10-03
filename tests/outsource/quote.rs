//! outsource quote 域集成测试（Phase 2 2026-09-13）
//!
//! 覆盖：
//! - create DRAFT happy path + DRAFT→SUBMITTED→APPROVED 状态机
//! - approve MANAGER-only（CLERK 拒绝 403）
//! - reject SUBMITTED → REJECTED（review_note 必填）
//! - update DRAFT（OCC 版本冲突）
//! - submit DRAFT only（SUBMITTED 状态再 submit → 400）
//! - soft-delete 仅 DRAFT / REJECTED 可删
//! - duplicate 同 (part, company, process) → 409
//! - list keyword 零命中 → 0 行
//! - list `customer_id`：L1 展开到全部 L2 子客户（无需 keyword）/ L2 精确 / 与 keyword
//!   取交集 / 零命中 → 0 行（2026-10-04）
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! 通用 `login_*` helper，统一走
//! `use hsh_erp_test_support::{...}` + `bootstrap_as_manager()` /
//! `bootstrap_as_clerk()` + `load_outsource_fixture(&pool)`。保留：
//! - `seed_outsource_process` / `insert_l1_customer` / `insert_part` /
//!   `insert_company` / `setup_basic`：quote 域独享（每个测试要按需造不同
//!   customer prefix / 不同 process code / 不同 company name 的组合；
//!   fixture 预置的 FX-OPROC-A 仅作 baseline 共享）；
//! - 域独享 helper 不从 `fixtures` 模块 `use`（Phase H gate 5 禁止）；
//!   本地 helper 用 `sqlx::query` 直插与 fixtures::* 同形 SQL。
//!
//! ## 不预置 t_outsource_quote / t_part / t_part_batch
//! 状态机不允许从 APPROVED 回退 DRAFT / REJECTED，且每个测试都要按需造不同
//! (part, company, process) 组合的 quote；预置会污染「期望空库」list 断言。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, send, test_app, test_pool,
    test_state,
};

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 outsource fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。绝大多数 quote 测试以 MANAGER 身份跑。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 起一份 fresh database + 加载 outsource fixture + 以 CLERK 身份登录。
///
/// 仅 `approve_quote_clerk_forbidden_40300` 使用，验证 CLERK 拒绝 approve。
async fn bootstrap_as_clerk() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  quote 域独享 helpers（绕开 fixtures::* 因为 Phase H gate 5 禁止从 `fixtures`
//  模块 use 任何动态 helper）
// ===========================================================================

/// 直插客户（L1）—— 绕开 customer CRUD。
async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, serial_prefix, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_customer");
    id
}

/// 直插 L2 客户（挂到 `parent_id` 之下）—— `customer_id` 子树展开用例的前提。
///
/// `serial_prefix` 唯一索引 `uq_t_customer_root_prefix` 只作用于 `parent_id IS NULL`
/// 的根客户，L2 不受限（这里干脆传 NULL）。
async fn insert_l2_customer(pool: &PgPool, name: &str, parent_id: i64) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(parent_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_customer (L2)");
    id
}

/// 直插 part（PENDING）—— 绕开 part CRUD。
async fn insert_part(pool: &PgPool, customer_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $4, 'PENDING', 0, $5, $5)",
    )
    .bind(id)
    .bind(format!("PT-{id}"))
    .bind(format!("DWG-{id}"))
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 直插 part 并在 name / drawing_no 里带上 tag —— keyword 维度用例要靠它区分零件。
async fn insert_tagged_part(pool: &PgPool, customer_id: i64, tag: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $4, 'PENDING', 0, $5, $5)",
    )
    .bind(id)
    .bind(format!("PT-{tag}"))
    .bind(format!("DWG-{tag}-{id}"))
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert tagged t_part");
    id
}

/// 直插 OUTSOURCE 类别 process。
async fn seed_outsource_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert OUTSOURCE process");
    id
}

/// 直插外协公司。
async fn insert_company(pool: &PgPool, name: &str, is_active: bool) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company \
         (id, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(is_active)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company");
    id
}

/// 完整准备：1 客户 + 1 part + 1 公司 + 1 OUTSOURCE process，返回 (pid, cid, proc_id)。
async fn setup_basic(pool: &PgPool) -> (i64, i64, i64) {
    let customer_id = insert_l1_customer(pool, "QuoteCo", "Q").await;
    let part_id = insert_part(pool, customer_id).await;
    let company_id = insert_company(pool, "Quote Outsource Co", true).await;
    let proc_id = seed_outsource_process(pool, "QPROC", "Q过程").await;
    (part_id, company_id, proc_id)
}

/// 建一条 DRAFT 报价（`customer_id` 过滤用例的造数入口）。
async fn create_quote(
    app: &axum::Router,
    token: &str,
    part_id: i64,
    company_id: i64,
    process_id: i64,
) -> (StatusCode, serde_json::Value) {
    send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": part_id.to_string(),
                "outsource_company_id": company_id.to_string(),
                "process_id": process_id.to_string(),
                "price": "3.00",
            })),
            Some(token),
        ),
    )
    .await
}

/// 取报价列表里的 `part_name` 集合（用于「命中了哪些零件」的断言）。
fn part_names(env: &serde_json::Value) -> Vec<String> {
    env["data"]["items"]
        .as_array()
        .expect("items 必须是数组")
        .iter()
        .map(|i| i["part_name"].as_str().unwrap_or_default().to_string())
        .collect()
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_quote_draft_happy() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "12.50",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    assert_eq!(env["data"]["status"], "DRAFT");
    assert_eq!(env["data"]["price"], "12.50");
}

/// `list_quotes` 的 keyword **零命中**必须返回 0 行（与 `sent-parts` 同源）。
///
/// 这条断言守着 SQL 谓词
/// `AND (cardinality($N::bigint[]) = 0 OR part_id = ANY($N))` 的一个陷阱：
/// `part_keyword_search` 零命中时给出空数组 → `cardinality = 0` 成立 →
/// keyword 条件被短路掉 → 返回**全量**报价（list 与 count 同时错）。
/// service 层早返回是唯一的兜底点。
#[tokio::test]
async fn list_quotes_keyword_zero_match_returns_empty_not_all_rows() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    // 同 (company, process) 下两台不同零件各挂 1 条报价 —— 同一 (part, company,
    // process) 二次 create 会 21303，所以必须换 part。
    let pid2 = insert_part(&pool, insert_l1_customer(&pool, "QuoteCo2", "R").await).await;
    for p in [pid, pid2] {
        let (s, env) = send(
            app.clone(),
            json_request(
                "POST",
                "/outsource-quotes",
                Some(json!({
                    "part_id": p.to_string(),
                    "outsource_company_id": cid.to_string(),
                    "process_id": proc_id.to_string(),
                    "price": "3.00",
                })),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{env}");
    }

    let list = |qs: &str, app: &axum::Router, token: String| {
        let url = format!("/outsource-quotes{qs}");
        let app = app.clone();
        async move {
            send(
                app.clone(),
                json_request("GET", &url, None, Some(token.as_str())),
            )
            .await
        }
    };

    // 无 keyword → 全量（对照组）
    let (s, env) = list("", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "无 keyword 应返回全量: {env}");

    // 零命中 keyword → 0 条。删掉 service 层早返回就会拿到 2 条 → 红。
    let (s, env) = list("?keyword=NOSUCHTOKENQQ", &app, token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "零命中 keyword 必须 total=0（曾返回全量 2）: {env}"
    );
    assert!(
        env["data"]["items"].as_array().unwrap().is_empty(),
        "零命中 keyword 必须 items 为空: {env}"
    );
}

#[tokio::test]
async fn create_quote_duplicate_returns_21303() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    let body = json!({
        "part_id": pid.to_string(),
        "outsource_company_id": cid.to_string(),
        "process_id": proc_id.to_string(),
        "price": "9.99",
    });
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(body.clone()),
            Some(&token),
        ),
    )
    .await;
    let (s2, env2) = send(
        app,
        json_request("POST", "/outsource-quotes", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "duplicate: {env2}");
    assert_eq!(env2["code"].as_i64().unwrap(), 21303);
}

#[tokio::test]
async fn quote_full_lifecycle_draft_submit_approve() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    // create
    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "20.00",
            })),
            Some(&token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    let ver = env_c["data"]["version"].as_i64().unwrap();

    // submit
    let (s_sub, env_sub) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_sub, StatusCode::OK, "submit: {env_sub}");
    assert_eq!(env_sub["data"]["status"], "SUBMITTED");
    assert!(env_sub["data"]["submitted_at"].is_string());

    // approve (MANAGER-only — token 是 manager)
    let (s_app, env_app) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/approve"),
            Some(json!({"review_note": "OK", "version": ver + 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_app, StatusCode::OK, "approve: {env_app}");
    assert_eq!(env_app["data"]["status"], "APPROVED");
    assert_eq!(env_app["data"]["review_note"], "OK");
}

#[tokio::test]
async fn approve_quote_clerk_forbidden_40300() {
    let (pool_mgr, app_mgr, m_token, _fx) = bootstrap_as_manager().await;
    let (_pool_clerk, app_clerk, c_token, _fx_clerk) = bootstrap_as_clerk().await;
    let (pid, cid, proc_id) = setup_basic(&pool_mgr).await;

    // create via manager
    let (_, env_c) = send(
        app_mgr.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "5.00",
            })),
            Some(&m_token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    // 跳过 submit（用 raw SQL 直接 set SUBMITTED，version+1 → 1）
    sqlx::query(
        "UPDATE t_outsource_quote SET status='SUBMITTED', submitted_at = now(), \
         version=version+1 WHERE id=$1",
    )
    .bind(qid.parse::<i64>().unwrap())
    .execute(&pool_mgr)
    .await
    .unwrap();

    // clerk 尝试 approve → 403
    let (s, env) = send(
        app_clerk,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/approve"),
            Some(json!({"version": 1})),
            Some(&c_token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "clerk approve: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40300);
}

#[tokio::test]
async fn reject_quote_requires_review_note() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "10.00",
            })),
            Some(&token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    // submit
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    // reject with empty note → 400
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/reject"),
            Some(json!({"review_note": "", "version": 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "empty note: {env}");
    // reject with note
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/reject"),
            Some(json!({"review_note": "太贵", "version": 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "reject: {env2}");
    assert_eq!(env2["data"]["status"], "REJECTED");
}

#[tokio::test]
async fn submit_quote_wrong_status_returns_21302() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;

    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "10.00",
            })),
            Some(&token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    // 第一次 submit OK
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    // 第二次 submit → 400 / 21302
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "2nd submit: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21302);
}

#[tokio::test]
async fn soft_delete_quote_approved_forbidden() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let (_, env_c) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-quotes",
            Some(json!({
                "part_id": pid.to_string(),
                "outsource_company_id": cid.to_string(),
                "process_id": proc_id.to_string(),
                "price": "10.00",
            })),
            Some(&token),
        ),
    )
    .await;
    let qid = env_c["data"]["id"].as_str().unwrap().to_string();
    // submit + approve → APPROVED
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            None,
            Some(&token),
        ),
    )
    .await;
    let (_, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/approve"),
            Some(json!({"version": 1})),
            Some(&token),
        ),
    )
    .await;
    // soft-delete → 400 / 21302
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "sd APPROVED: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21302);
}

// ===========================================================================
//  `customer_id` 过滤（2026-10-04 新增语义）
// ===========================================================================

/// 只给 `customer_id`（**L1**）就能命中其全部 L2 子客户的报价，**不需 keyword**。
///
/// 2026-10-04 之前 service 把 `customer_id` 解析成 `_cid` 后直接丢弃，且「给了
/// `customer_id` 没给 `keyword`」就早返回空列表 ⇒ 前端选客户后一览恒空。
#[tokio::test]
async fn list_quotes_customer_id_l1_expands_to_children_without_keyword() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1_customer(&pool, "QCustRoot", "W").await;
    let l2_a = insert_l2_customer(&pool, "QCustA", l1).await;
    let l2_b = insert_l2_customer(&pool, "QCustB", l1).await;
    // 另一个 L1 下的叶子：不在本 L1 子树内，必须被排除
    let other_l1 = insert_l1_customer(&pool, "QCustOther", "V").await;
    let other_l2 = insert_l2_customer(&pool, "QCustC", other_l1).await;

    let proc_id = seed_outsource_process(&pool, "QCL1", "QC-L1").await;
    let company = insert_company(&pool, "QCustCo", true).await;
    for (cid, tag) in [(l2_a, "ALPHA"), (l2_b, "BETA"), (other_l2, "GAMMA")] {
        let pid = insert_tagged_part(&pool, cid, tag).await;
        let (s, env) = create_quote(&app, &token, pid, company, proc_id).await;
        assert_eq!(s, StatusCode::CREATED, "{env}");
    }

    let list = |qs: &str, app: &axum::Router, token: String| {
        let url = format!("/outsource-quotes{qs}");
        let app = app.clone();
        async move {
            send(
                app.clone(),
                json_request("GET", &url, None, Some(token.as_str())),
            )
            .await
        }
    };

    // 对照：不带任何过滤 → 3 条
    let (s, env) = list("", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 3, "{env}");

    // 只给 L1 → 2 条（两个 L2 子客户的零件），GAMMA 属于别的 L1 子树
    let (s, env) = list(&format!("?customer_id={l1}"), &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 2,
        "只给 L1 必须命中其全部 L2 子客户且无需 keyword: {env}"
    );
    let names = part_names(&env);
    assert!(names.iter().any(|n| n == "PT-ALPHA"), "{names:?}");
    assert!(names.iter().any(|n| n == "PT-BETA"), "{names:?}");
    assert!(!names.iter().any(|n| n == "PT-GAMMA"), "{names:?}");
}

/// 只给 `customer_id`（**L2**）→ 只返回该 L2 的报价（等值那一支必须保留）。
#[tokio::test]
async fn list_quotes_customer_id_l2_returns_only_its_own_quotes() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1_customer(&pool, "QCustRoot2", "U").await;
    let l2_a = insert_l2_customer(&pool, "QCustA2", l1).await;
    let l2_b = insert_l2_customer(&pool, "QCustB2", l1).await;
    let proc_id = seed_outsource_process(&pool, "QCL2", "QC-L2").await;
    let company = insert_company(&pool, "QCustCo2", true).await;
    for (cid, tag) in [(l2_a, "ALPHA"), (l2_b, "BETA")] {
        let pid = insert_tagged_part(&pool, cid, tag).await;
        let (s, env) = create_quote(&app, &token, pid, company, proc_id).await;
        assert_eq!(s, StatusCode::CREATED, "{env}");
    }

    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-quotes?customer_id={l2_a}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "传 L2 只返回该 L2 的: {env}");
    assert_eq!(part_names(&env), vec!["PT-ALPHA".to_string()], "{env}");
}

/// `customer_id` + `keyword` 同时给 → **取交集**（service 层 `HashSet` 求交）。
#[tokio::test]
async fn list_quotes_customer_id_and_keyword_intersection() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1_customer(&pool, "QCustRoot3", "T").await;
    let l2_a = insert_l2_customer(&pool, "QCustA3", l1).await;
    let l2_b = insert_l2_customer(&pool, "QCustB3", l1).await;
    // 关键字命中的零件挂在别的 L1 下 ⇒ 交集必为空
    let other_l1 = insert_l1_customer(&pool, "QCustOther3", "S").await;
    let other_l2 = insert_l2_customer(&pool, "QCustC3", other_l1).await;
    let proc_id = seed_outsource_process(&pool, "QCL3", "QC-L3").await;
    let company = insert_company(&pool, "QCustCo3", true).await;
    for (cid, tag) in [(l2_a, "ALPHA"), (l2_b, "BETA"), (other_l2, "GAMMA")] {
        let pid = insert_tagged_part(&pool, cid, tag).await;
        let (s, env) = create_quote(&app, &token, pid, company, proc_id).await;
        assert_eq!(s, StatusCode::CREATED, "{env}");
    }

    // 交集命中：L1 子树 ∩ keyword=ALPHA
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-quotes?customer_id={l1}&keyword=ALPHA"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "交集必须只留 1 条: {env}");
    assert_eq!(part_names(&env), vec!["PT-ALPHA".to_string()], "{env}");

    // keyword 单独给 → 全库 1 条（证明上面的 1 不是 keyword 的功劳）
    let (s, env) = send(
        app.clone(),
        json_request("GET", "/outsource-quotes?keyword=ALPHA", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");

    // 交集为空：keyword=GAMMA 的零件不在 L1 子树内 ⇒ total 0（不是全量 3）
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-quotes?customer_id={l1}&keyword=GAMMA"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "交集为空必须 total=0（曾返回全量）: {env}"
    );
}

/// 只给一个零命中的 `customer_id` → total 0，**不是全量**。
///
/// 守着 SQL 谓词 `AND (cardinality($4::bigint[]) = 0 OR part_id = ANY($4))` 的陷阱：
/// 展开成空数组后 `cardinality = 0` 成立、整个条件被短路。service 层的零命中早返回
/// 守卫必须把 customer 维度也算进去（判定条件 `kw.is_some() || cid_given`），
/// 否则「选了一个零件都没有的客户」会列出全部报价。
#[tokio::test]
async fn list_quotes_customer_id_zero_match_returns_empty_not_all_rows() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let pid2 = insert_part(&pool, insert_l1_customer(&pool, "QuoteCo2", "R").await).await;
    for p in [pid, pid2] {
        let (s, env) = create_quote(&app, &token, p, cid, proc_id).await;
        assert_eq!(s, StatusCode::CREATED, "{env}");
    }

    // 一个存在但**没有任何零件**的客户（底下也没有子客户）
    let empty_root = insert_l1_customer(&pool, "QuoteEmptyRoot", "E").await;
    insert_l2_customer(&pool, "QuoteEmptyLeaf", empty_root).await;

    let (s, env) = send(
        app.clone(),
        json_request("GET", "/outsource-quotes", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "对照组：全量 2 条: {env}");

    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-quotes?customer_id={empty_root}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "零命中的 customer_id 必须 total=0（曾返回全量 2）: {env}"
    );
    assert!(env["data"]["items"].as_array().unwrap().is_empty(), "{env}");
}
