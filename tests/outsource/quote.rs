//! outsource quote 域集成测试（Phase 2 2026-09-13）
//!
//! 覆盖：
//! - create DRAFT happy path + DRAFT→SUBMITTED→APPROVED 状态机
//! - approve MANAGER-only（CLERK 拒绝 403）
//! - reject SUBMITTED → REJECTED（review_note 必填）
//! - submit DRAFT only（SUBMITTED 状态再 submit → 400）+ `version` 必填
//! - soft-delete 仅 DRAFT / REJECTED 可删 + `version` 必填
//! - duplicate 同 (part, company, process) → 409
//! - list `statuses[]`：传则过滤、不传不过滤（2026-09 前 DTO 缺字段 ⇒ 恒不过滤）
//! - list `drawing_no` / `name` / `is_urgent`：直连 ILIKE / 精确谓词
//! - list `customer_id`：L1 展开到全部 L2 子客户（无需零件侧筛选）/ L2 精确 /
//!   零件侧维度取交集 / 零命中 → 0 行（2026-10-04）
//! - `GET /{id}` 与 `POST /{id}/update` 已硬切下线（404）
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
    OutsourceFixture, json_request, load_outsource_fixture, login_token, send, send_raw, test_app,
    test_pool, test_state,
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

/// 直插 part 并在 name / drawing_no 里带上 tag —— `drawing_no` / `name` 维度用例要靠它区分零件。
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

/// 直插 part（可指定 `is_urgent`）—— `is_urgent` 筛选维度用例用。
async fn insert_urgent_part(pool: &PgPool, customer_id: i64, tag: &str, is_urgent: bool) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, is_urgent, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $4, $5, 'PENDING', 0, $6, $6)",
    )
    .bind(id)
    .bind(format!("PT-{tag}"))
    .bind(format!("DWG-{tag}-{id}"))
    .bind(customer_id)
    .bind(is_urgent)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert urgent t_part");
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

/// 提交一条 DRAFT 报价（`version` 必填，2026-10-09 起）。
///
/// 传 `version = 0` 对应刚建出来的行；本文件多数用例在 create 之后只发生一次状态
/// 流转，所以 0 就是当下的真值。
async fn submit_quote(
    app: &axum::Router,
    token: &str,
    qid: &str,
    version: i64,
) -> (StatusCode, serde_json::Value) {
    send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            Some(json!({ "version": version })),
            Some(token),
        ),
    )
    .await
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

/// `list_quotes` 的 `drawing_no` / `name` **零命中**必须返回 0 行。
///
/// 2026-10-09 起这两个维度是**直连 ILIKE 谓词**（`($6::text IS NULL OR p.drawing_no
/// ILIKE $6)`），零命中在 SQL 里自然就是零行 —— 不再依赖 service 层那个「给了关键词却
/// 零命中要早返回」的分支（它是旧 `part_keyword_search` 预搜索留下的补偿逻辑：空数组
/// 会让 `cardinality($2) = 0` 成立、整个条件被短路，从而返回全量）。本用例守着
/// 「直连谓词 + 空数组语义不再被复用」这个事实。
#[tokio::test]
async fn list_quotes_part_filters_zero_match_returns_empty_not_all_rows() {
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

    // 不传零件侧筛选 → 全量（对照组）
    let (s, env) = list("", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "不传筛选应返回全量: {env}");

    // 零命中 drawing_no → 0 条
    let (s, env) = list("?drawing_no=NOSUCHTOKENQQ", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "零命中 drawing_no 必须 total=0（曾返回全量 2）: {env}"
    );
    assert!(
        env["data"]["items"].as_array().unwrap().is_empty(),
        "零命中 drawing_no 必须 items 为空: {env}"
    );

    // 零命中 name → 0 条
    let (s, env) = list("?name=NOSUCHTOKENQQ", &app, token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "零命中 name 必须 total=0: {env}");
}

/// `drawing_no` / `name` / `is_urgent` 三个零件侧维度各自的命中与排除。
#[tokio::test]
async fn list_quotes_part_filters_are_applied() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cust = insert_l1_customer(&pool, "PartDimCust", "M").await;
    let proc_id = seed_outsource_process(&pool, "QPD", "Q-PartDim").await;
    let company = insert_company(&pool, "PartDimCo", true).await;
    // 急件（HOT，name 含 HOT）与常件（COLD）
    let hot = insert_urgent_part(&pool, cust, "HOT", true).await;
    let cold = insert_urgent_part(&pool, cust, "COLD", false).await;
    for p in [hot, cold] {
        let (s, env) = create_quote(&app, &token, p, company, proc_id).await;
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

    // drawing_no 命中只有 HOT（DWG-HOT-… 含 HOT）
    let (s, env) = list("?drawing_no=DWG-HOT", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert_eq!(part_names(&env), vec!["PT-HOT".to_string()], "{env}");

    // name 命中只有 COLD
    let (s, env) = list("?name=PT-COLD", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(part_names(&env), vec!["PT-COLD".to_string()], "{env}");

    // is_urgent=true 只有 HOT
    let (s, env) = list("?is_urgent=true", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(part_names(&env), vec!["PT-HOT".to_string()], "{env}");

    // is_urgent=false 只有 COLD
    let (s, env) = list("?is_urgent=false", &app, token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(part_names(&env), vec!["PT-COLD".to_string()], "{env}");
}

/// 🔴 `statuses` 筛选必须真的生效。
///
/// 这是本域最隐蔽的一个 bug 的回归：SQL（`AND (cardinality($2::text[]) = 0 OR status
/// = ANY($2))`）与 repo（`statuses: &[String]` 形参）两层**早就支持**多状态，缺的是 DTO
/// 字段与 service 接线。症状是**静默**的：前端一直发状态筛选参数，DTO 没有对应字段 ⇒
/// serde 忽略未知 query 参数（不报错）⇒ 状态筛选恒不生效 —— 连前端的角色默认筛选
/// （MANAGER → `['SUBMITTED']`、CLERK → `['DRAFT']`）也没生效，所以 MANAGER 打开报价
/// 一览看到的是全量报价，且表头因 `statusFilterActive` 判定为「有筛选」而变蓝加粗，
/// 视觉上在说筛选已生效。
///
/// ⚠️ wire format 是**逗号分隔单值**（`?statuses=SUBMITTED`），不是重复 key：axum 的
/// `Query` 走 `serde_urlencoded`，它的 `Part` 反序列化器不支持序列 —— 重复 key 形态会
/// 400（`invalid type: string "DRAFT", expected a sequence`）。
#[tokio::test]
async fn list_quotes_statuses_filter_is_applied() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let (s, env) = create_quote(&app, &token, pid, cid, proc_id).await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let qid = env["data"]["id"].as_str().unwrap().to_string();
    // 提交 → SUBMITTED
    let (s, env) = submit_quote(&app, &token, &qid, 0).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    // 再造一条留在 DRAFT 的（同 company / process，必须换 part）
    let pid2 = insert_part(&pool, insert_l1_customer(&pool, "StCust", "N").await).await;
    let (s, env) = create_quote(&app, &token, pid2, cid, proc_id).await;
    assert_eq!(s, StatusCode::CREATED, "{env}");

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

    // 不传 statuses → 不过滤（对照组，全量 2）
    let (s, env) = list("", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "不传 statuses 应不过滤: {env}");

    // statuses[]=SUBMITTED → 只剩 1 条
    let (s, env) = list("?statuses=SUBMITTED", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 1,
        "statuses=SUBMITTED 必须过滤掉 DRAFT（曾恒返全量）: {env}"
    );
    assert_eq!(env["data"]["items"][0]["status"], "SUBMITTED", "{env}");

    // statuses=DRAFT → 只剩 1 条，且与上一条互补
    let (s, env) = list("?statuses=DRAFT", &app, token.clone()).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert_eq!(env["data"]["items"][0]["status"], "DRAFT", "{env}");

    // CSV 多值 → 两条都出（证明是 ANY 而不是「只认第一个」）。
    // 注：逗号写成 %2C 是因为 `serde_urlencoded` 的 `Part` 反序列化器**不支持序列**，
    // `?statuses=DRAFT&statuses=SUBMITTED` 那种重复 key 形态会 400（见 DTO 注释）。
    let (s, env) = list("?statuses=DRAFT%2CSUBMITTED", &app, token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 2,
        "CSV 多值 statuses 应命中 2 条: {env}"
    );
}

/// `status`（单值）与 `statuses`（多值）是**两个并存**的维度，AND 生效。
#[tokio::test]
async fn list_quotes_status_and_statuses_are_anded() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let (s, env) = create_quote(&app, &token, pid, cid, proc_id).await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let qid = env["data"]["id"].as_str().unwrap().to_string();
    let (s, env) = submit_quote(&app, &token, &qid, 0).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let pid2 = insert_part(&pool, insert_l1_customer(&pool, "AndCust", "O").await).await;
    let (s, env) = create_quote(&app, &token, pid2, cid, proc_id).await;
    assert_eq!(s, StatusCode::CREATED, "{env}");

    // status=SUBMITTED AND statuses=SUBMITTED → 1 条
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            "/outsource-quotes?status=SUBMITTED&statuses=SUBMITTED",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");

    // status=SUBMITTED AND statuses=DRAFT → 0 条（AND，不是 OR 也不是覆盖）
    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/outsource-quotes?status=SUBMITTED&statuses=DRAFT",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "{env}");
}

/// `GET /{id}` 与 `POST /{id}/update` 已于 2026-10-09 硬切下线（前端零消费，无 alias）。
///
/// 用 `send_raw`：matchit 的 404 fallback 是**空 body**，`send` 的 JSON 解析会 panic。
#[tokio::test]
async fn quote_detail_and_update_endpoints_are_gone() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let (s, env) = create_quote(&app, &token, pid, cid, proc_id).await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let qid = env["data"]["id"].as_str().unwrap().to_string();

    // `GET /{id}`：删掉后本 router 无任何 1 段路由 ⇒ 404
    let (s, _) = send_raw(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-quotes/{qid}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "GET /{{id}} 必须已硬切下线（404）"
    );

    // `POST /{id}/update`
    let (s, _) = send_raw(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/update"),
            Some(json!({"price": "1.00", "version": 0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "POST /{{id}}/update 必须已硬切下线（404）"
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
    let (s_sub, env_sub) = submit_quote(&app, &token, &qid, ver).await;
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
    let (s, env) = submit_quote(&app, &token, &qid, 0).await;
    assert_eq!(s, StatusCode::OK, "submit: {env}");
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
    let (s, env) = submit_quote(&app, &token, &qid, 0).await;
    assert_eq!(s, StatusCode::OK, "1st submit: {env}");
    // 第二次 submit → 400 / 21302（version 传 1，即 submit 之后的真实版本；
    // **状态机守卫在 OCC 之前**，所以传对传错都是 21302）
    let (s, env) = submit_quote(&app, &token, &qid, 1).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "2nd submit: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21302);
}

/// `submit` 的 `version` 必填：缺字段 ⇒ axum `422` 纯文本，不进 `R<T>` 信封。
///
/// 用 `send_raw`：axum 的 `Json` 反序列化拒绝是纯文本 body，`send` 会在 JSON 解析处 panic。
#[tokio::test]
async fn submit_quote_requires_version_field() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let (s, env) = create_quote(&app, &token, pid, cid, proc_id).await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let qid = env["data"]["id"].as_str().unwrap().to_string();

    let (s, body) = send_raw(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/submit"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "缺 version 必须是 422（不是业务信封）: {body}"
    );
    assert!(
        !body.contains("\"code\""),
        "缺字段是 axum 的纯文本拒绝，不得有 code 字段: {body}"
    );
    assert!(
        body.contains("missing field `version`"),
        "错误正文应指出缺 version: {body}"
    );
}

/// `submit` 的 OCC：传过期 `version` ⇒ 40901。
///
/// 守卫必须用**调用方传的** version。此前 service 是「先 `quote_get_by_id` 读到当前
/// version 再喂给 `quote_submit`」，等于用服务端自己读到的值守自己的乐观锁 ——
/// `UPDATE … WHERE version = <刚读的>` 在同一行上恒成立，守卫形同虚设。
#[tokio::test]
async fn submit_quote_version_conflict_returns_40901() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let (s, env) = create_quote(&app, &token, pid, cid, proc_id).await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let qid = env["data"]["id"].as_str().unwrap().to_string();

    let (s, env) = submit_quote(&app, &token, &qid, 99).await;
    assert_eq!(s, StatusCode::CONFLICT, "过期 version 必须 409: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40901, "{env}");
}

/// `soft-delete` 的 `version` 必填 + OCC 生效。
#[tokio::test]
async fn soft_delete_quote_requires_and_guards_version() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (pid, cid, proc_id) = setup_basic(&pool).await;
    let (s, env) = create_quote(&app, &token, pid, cid, proc_id).await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let qid = env["data"]["id"].as_str().unwrap().to_string();

    // 缺 version → 422（axum 纯文本，用 send_raw）
    let (s, body) = send_raw(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/soft-delete"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("missing field `version`"), "{body}");

    // 过期 version → 40901
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/soft-delete"),
            Some(json!({"version": 99})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40901, "{env}");

    // 正确 version → 200，且行被软删（列表里看不到）
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/soft-delete"),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert!(env["data"].is_null(), "{env}");
    let (s, env) = send(
        app,
        json_request("GET", "/outsource-quotes", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "软删后不应再出现: {env}");
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
    let (s, env) = submit_quote(&app, &token, &qid, 0).await;
    assert_eq!(s, StatusCode::OK, "submit: {env}");
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
    // soft-delete → 400 / 21302（**状态机守卫在 OCC 之前**，故 version 传 2 无妨）
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-quotes/{qid}/soft-delete"),
            Some(json!({"version": 2})),
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

/// 只给 `customer_id`（**L1**）就能命中其全部 L2 子客户的报价，**不需零件侧筛选**。
///
/// 2026-10-04 之前 service 把 `customer_id` 解析成 `_cid` 后直接丢弃，且「给了
/// `customer_id` 没给 `keyword`」就早返回空列表 ⇒ 前端选客户后一览恒空。
#[tokio::test]
async fn list_quotes_customer_id_l1_expands_to_children_without_part_filter() {
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
        "只给 L1 必须命中其全部 L2 子客户且无需零件侧筛选: {env}"
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

/// `customer_id` 与 `drawing_no` 同时给 → **AND**（各占各的 WHERE 段，由 DB 求交）。
///
/// 2026-10-09 之前是 service 层拿两个 part_id 集合做 `HashSet` 求交；拆成直连 ILIKE
/// 之后零件侧谓词直接落在 SQL 上，交集由 PG 求，交集语义不变、但中间集合与那段求交
/// 代码一起消失。
#[tokio::test]
async fn list_quotes_customer_id_and_drawing_no_intersection() {
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

    // 交集命中：L1 子树 ∩ drawing_no 含 ALPHA
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-quotes?customer_id={l1}&drawing_no=ALPHA"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "交集必须只留 1 条: {env}");
    assert_eq!(part_names(&env), vec!["PT-ALPHA".to_string()], "{env}");

    // drawing_no 单独给 → 全库 1 条（证明上面的 1 不是 drawing_no 的功劳）
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            "/outsource-quotes?drawing_no=ALPHA",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");

    // 交集为空：GAMMA 的零件不在 L1 子树内 ⇒ total 0（不是全量 3）
    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/outsource-quotes?customer_id={l1}&drawing_no=GAMMA"),
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
