//! outsource 读侧 shipment list 集成测试（2026-10-03 新增）
//!
//! 覆盖：
//! - `GET /outsource-companies/{id}/sent-parts`：happy path（OUTSOURCING + RECEIVED
//!   都出现）/ 信封带公司 id + 名 / 行投影瘦身（无 `quote_id` / `part_id`）/
//!   `drawing_no` 过滤（命中 + **零命中返回 0 行**）/ `customer_id` / `process_id` /
//!   `is_billed` 三个精确维度 / sent_at 日期窗 / 3 种 sort_by / sort 白名单大小写
//!   不敏感 / 非法 sort_by 回落并**验序** / 分页 total+offset
//! - `GET /outsource-shipments/in-flight`：只返 OUTSOURCING；**`version` 取
//!   `t_part_batch.version` 而非 `t_outsource_shipment.version`**（专门用两个不同值
//!   区分）；**`quantity` 取 `t_part_batch.quantity` 而非 `shipment.quantity`**
//!
//! ## 范本
//! 沿用 `quote.rs` / `send_receive.rs` 的 PR13 Phase H 风格：通用 helper 走
//! `hsh_erp_test_support`，域独享 helper 用本地 `sqlx::query` 直插。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_test_support::{
    OutsourceFixture, PartFixture, json_request, load_outsource_fixture, login_token, send,
    test_app, test_pool, test_state,
};

// ===========================================================================
//  Bootstrap
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 以 INSPECTOR 身份登录（用户名复用 part 域基线 fixture 的 INSPECTOR 用户；
/// 该用户在 `load_outsource_fixture` 里随 part fixture 一起落库）。
async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String) {
    let pool = test_pool().await;
    load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(
        &app,
        PartFixture::INSPECTOR_USERNAME,
        OutsourceFixture::PASSWORD,
    )
    .await;
    (pool, app, token)
}

// ===========================================================================
//  域独享 helpers
// ===========================================================================

async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
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

/// 直插 part；`drawing_tag` 便于 `drawing_no` 断言区分。
async fn insert_part(pool: &PgPool, customer_id: i64, drawing_tag: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, is_urgent, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $4, $5, 'PENDING', 0, $6, $6)",
    )
    .bind(id)
    .bind(format!("NAME-{drawing_tag}"))
    .bind(format!("DWG-{drawing_tag}-{id}"))
    .bind(customer_id)
    .bind(drawing_tag == "URGENT")
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

async fn seed_outsource_process(pool: &PgPool, code: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(format!("proc-{code}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

/// 直插 shipment。`sent_at` / `received_at` 显式给值，便于日期窗断言。
#[allow(clippy::too_many_arguments)]
async fn insert_shipment(
    pool: &PgPool,
    quote_id: i64,
    part_id: i64,
    batch_id: Option<i64>,
    company_id: i64,
    process_id: i64,
    quantity: i32,
    unit_price: &str,
    status: &str,
    sent_at: &str,
    received_at: Option<&str>,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_shipment \
         (id, quote_id, part_id, batch_id, outsource_company_id, process_id, quantity, \
          unit_price, status, sent_at, received_at, is_billed, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::numeric, $9, $10::timestamp, \
                 $11::timestamp, false, 0, now(), now())",
    )
    .bind(id)
    .bind(quote_id)
    .bind(part_id)
    .bind(batch_id)
    .bind(company_id)
    .bind(process_id)
    .bind(quantity)
    .bind(unit_price)
    .bind(status)
    .bind(sent_at)
    .bind(received_at)
    .execute(pool)
    .await
    .expect("insert t_outsource_shipment");
    id
}

async fn insert_quote(pool: &PgPool, part_id: i64, company_id: i64, process_id: i64) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_quote \
         (id, part_id, outsource_company_id, process_id, price, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 10.00, 'APPROVED', 0, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(company_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert t_outsource_quote");
    id
}

/// 直插批次（可指定 version / quantity —— in-flight 断言要靠它们与 shipment 区分）。
async fn insert_batch(pool: &PgPool, part_id: i64, quantity: i32, version: i32) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_part_batch \
         (id, part_id, batch_no, quantity, status, location, version, created_at, updated_at) \
         VALUES ($1, $2, 1, $3, 'OUTSOURCE', 'OUTSOURCE_COMPANY', $4, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(quantity)
    .bind(version)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// sent-parts 的通用调用：返回 (status, env)。
async fn get_sent_parts(
    app: &axum::Router,
    token: &str,
    company_id: i64,
    qs: &str,
) -> (StatusCode, serde_json::Value) {
    send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-companies/{company_id}/sent-parts{qs}"),
            None,
            Some(token),
        ),
    )
    .await
}

/// 把一条 shipment 标成「已开票」（`insert_shipment` 一律写 `is_billed = false`，
/// 所以 `is_billed` 筛选维度用得着单独一条 UPDATE）。
async fn mark_billed(pool: &PgPool, shipment_id: i64) {
    sqlx::query("UPDATE t_outsource_shipment SET is_billed = true WHERE id = $1")
        .bind(shipment_id)
        .execute(pool)
        .await
        .expect("mark shipment billed");
}

/// 取零件的 `drawing_no`（`?drawing_no=` 断言用，避免拼 id）。
async fn part_drawing_no(pool: &PgPool, part_id: i64) -> String {
    sqlx::query_scalar("SELECT drawing_no FROM t_part WHERE id = $1")
        .bind(part_id)
        .fetch_one(pool)
        .await
        .expect("读 part drawing_no")
}

// ===========================================================================
//  sent-parts
// ===========================================================================

#[tokio::test]
async fn sent_parts_lists_outsourcing_and_received() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "SpCo", "U").await;
    let p1 = insert_part(&pool, cid, "A").await;
    let p2 = insert_part(&pool, cid, "B").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "SPPROC").await;
    let q1 = insert_quote(&pool, p1, company, proc_id).await;
    let q2 = insert_quote(&pool, p2, company, proc_id).await;
    insert_shipment(
        &pool,
        q1,
        p1,
        None,
        company,
        proc_id,
        3,
        "2.50",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;
    insert_shipment(
        &pool,
        q2,
        p2,
        None,
        company,
        proc_id,
        4,
        "3.00",
        "RECEIVED",
        "2026-09-02 10:00:00",
        Some("2026-09-05 10:00:00"),
    )
    .await;

    let (s, env) = get_sent_parts(&app, &token, company, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "{env}");
    let items = env["data"]["items"].as_array().unwrap();
    // 主键字段名是 shipment_id（不是 id）
    assert!(items.iter().all(|i| i["shipment_id"].is_string()), "{env}");
    let statuses: Vec<&str> = items
        .iter()
        .map(|i| i["status"].as_str().unwrap())
        .collect();
    assert!(statuses.contains(&"OUTSOURCING"), "{env}");
    assert!(statuses.contains(&"RECEIVED"), "{env}");
    // customer_path 真算出来了（此前恒 null → 前端「客户」列全 —）
    assert!(items.iter().all(|i| i["customer_path"] == "SpCo"), "{env}");
    // total_price = unit_price * quantity
    let found = items
        .iter()
        .find(|i| i["status"] == "OUTSOURCING")
        .expect("OUTSOURCING row");
    assert_eq!(found["unit_price"], "2.50", "{env}");
    assert_eq!(found["total_price"], "7.50", "{env}");

    // 2026-10-09：信封带公司 id + 名（前端渲染页头，不必再单独 GET 一次公司详情）
    assert_eq!(
        env["data"]["outsource_company_id"],
        company.to_string(),
        "{env}"
    );
    let company_name: String = sqlx::query_scalar(
        "SELECT name FROM t_outsource_company WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(company)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(env["data"]["outsource_company_name"], company_name, "{env}");

    // 2026-10-09：行投影删 `quote_id` / `part_id`（18 → 16 字段）
    assert_eq!(items[0].as_object().unwrap().len(), 16, "{env}");
    assert!(
        items[0].get("quote_id").is_none() && items[0].get("part_id").is_none(),
        "sent-parts 行不得再带 quote_id / part_id: {env}"
    );
}

/// 公司被软删后，信封的 `outsource_company_name` 必须是 `null`（端点**不**因此 404）。
///
/// 这是本端点的容错口径：它按 company_id 查 shipment 行，公司行消失只影响标题位，
/// 不该让整页数据读不出来。
#[tokio::test]
async fn sent_parts_envelope_company_name_null_when_company_soft_deleted() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    // 软删 fixture 公司（直接 UPDATE，绕过「仍映射工序」守卫）
    sqlx::query("UPDATE t_outsource_company SET deleted_at = now() WHERE id = $1")
        .bind(company)
        .execute(&pool)
        .await
        .expect("soft delete fixture company");

    let (s, env) = get_sent_parts(&app, &token, company, "").await;
    assert_eq!(s, StatusCode::OK, "公司已软删不得让端点 404: {env}");
    assert_eq!(
        env["data"]["outsource_company_id"],
        company.to_string(),
        "{env}"
    );
    assert!(
        env["data"]["outsource_company_name"].is_null(),
        "公司已软删时公司名必须是 null: {env}"
    );
}

/// `drawing_no` 直连 ILIKE 过滤：命中只有匹配的那一行。
#[tokio::test]
async fn sent_parts_drawing_no_filter_hits_only_matching_part() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "KwCo", "V").await;
    let hit = insert_part(&pool, cid, "MATCHME").await;
    let miss = insert_part(&pool, cid, "OTHER").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "KWPROC").await;
    let q1 = insert_quote(&pool, hit, company, proc_id).await;
    let q2 = insert_quote(&pool, miss, company, proc_id).await;
    insert_shipment(
        &pool,
        q1,
        hit,
        None,
        company,
        proc_id,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;
    insert_shipment(
        &pool,
        q2,
        miss,
        None,
        company,
        proc_id,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;

    let (s, env) = get_sent_parts(&app, &token, company, "?drawing_no=MATCHME").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    // 行投影已删 `part_id`，改用 `part_drawing_no` 定位
    assert_eq!(
        env["data"]["items"][0]["part_drawing_no"],
        part_drawing_no(&pool, hit).await,
        "{env}"
    );
}

/// 2026-10-09 新增的三个精确筛选维度：`customer_id` / `process_id` / `is_billed`。
///
/// `customer_id` 与报价一览**语义不同**：那边是「自身 ∪ 直接子客户」的客户子树展开，
/// 这边只判 `t_part.customer_id` 等值（对账时按「这家外协厂供过哪个客户的货」筛）。
#[tokio::test]
async fn sent_parts_exact_filters_customer_process_is_billed() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cust_a = insert_l1_customer(&pool, "ExA", "X").await;
    let cust_b = insert_l1_customer(&pool, "ExB", "Y").await;
    let proc_a = seed_outsource_process(&pool, "EXPA").await;
    let proc_b = seed_outsource_process(&pool, "EXPB").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let p_a = insert_part(&pool, cust_a, "A").await;
    let p_b = insert_part(&pool, cust_b, "B").await;
    let q1 = insert_quote(&pool, p_a, company, proc_a).await;
    let q2 = insert_quote(&pool, p_b, company, proc_b).await;
    // A 行：proc_a + 未开票；B 行：proc_b + 已开票
    insert_shipment(
        &pool,
        q1,
        p_a,
        None,
        company,
        proc_a,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;
    let sid_b = insert_shipment(
        &pool,
        q2,
        p_b,
        None,
        company,
        proc_b,
        1,
        "2.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;
    mark_billed(&pool, sid_b).await;

    let names = |env: &serde_json::Value| -> Vec<String> {
        env["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["part_drawing_no"].as_str().unwrap().to_string())
            .collect()
    };

    // 对照组：不传任何精确维度 → 2 条
    let (s, env) = get_sent_parts(&app, &token, company, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "{env}");

    // customer_id 等值
    let (s, env) = get_sent_parts(&app, &token, company, &format!("?customer_id={cust_a}")).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert_eq!(
        names(&env),
        vec![part_drawing_no(&pool, p_a).await],
        "{env}"
    );

    // process_id 等值
    let (s, env) = get_sent_parts(&app, &token, company, &format!("?process_id={proc_b}")).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        names(&env),
        vec![part_drawing_no(&pool, p_b).await],
        "{env}"
    );

    // is_billed=true / false 互补
    let (s, env) = get_sent_parts(&app, &token, company, "?is_billed=true").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        names(&env),
        vec![part_drawing_no(&pool, p_b).await],
        "{env}"
    );
    let (s, env) = get_sent_parts(&app, &token, company, "?is_billed=false").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        names(&env),
        vec![part_drawing_no(&pool, p_a).await],
        "{env}"
    );

    // 多维度 AND：cust_a + proc_b（跨行）→ 0 条
    let (s, env) = get_sent_parts(
        &app,
        &token,
        company,
        &format!("?customer_id={cust_a}&process_id={proc_b}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "多维度必须 AND: {env}");
}

#[tokio::test]
async fn sent_parts_sent_from_sent_to_window_excludes_outside() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "WinCo", "W").await;
    let p1 = insert_part(&pool, cid, "EARLY").await;
    let p2 = insert_part(&pool, cid, "LATE").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "WNPROC").await;
    let q1 = insert_quote(&pool, p1, company, proc_id).await;
    let q2 = insert_quote(&pool, p2, company, proc_id).await;
    insert_shipment(
        &pool,
        q1,
        p1,
        None,
        company,
        proc_id,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;
    insert_shipment(
        &pool,
        q2,
        p2,
        None,
        company,
        proc_id,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-20 10:00:00",
        None,
    )
    .await;

    // 闭区间：含两端
    let (s, env) = get_sent_parts(
        &app,
        &token,
        company,
        "?sent_from=2026-09-01T00:00:00&sent_to=2026-09-10T00:00:00",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    // 行投影已删 `part_id`，改用 `part_drawing_no` 定位
    assert_eq!(
        env["data"]["items"][0]["part_drawing_no"],
        part_drawing_no(&pool, p1).await,
        "{env}"
    );
}

#[tokio::test]
async fn sent_parts_sort_by_price_sent_at_received_at() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "SortCo", "X").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "SRPROC").await;
    // 三行：单价递增、发出时间递增、接收时间递减
    for (i, (tag, price, sent, recv)) in [
        (
            "C1",
            "9.00",
            "2026-09-03 10:00:00",
            Some("2026-09-10 10:00:00"),
        ),
        (
            "C2",
            "1.00",
            "2026-09-01 10:00:00",
            Some("2026-09-20 10:00:00"),
        ),
        (
            "C3",
            "5.00",
            "2026-09-02 10:00:00",
            Some("2026-09-15 10:00:00"),
        ),
    ]
    .iter()
    .enumerate()
    {
        let pid = insert_part(&pool, cid, tag).await;
        let qid = insert_quote(&pool, pid, company, proc_id).await;
        insert_shipment(
            &pool, qid, pid, None, company, proc_id, 1, price, "RECEIVED", sent, *recv,
        )
        .await;
        let _ = i;
    }

    let draw_names = |env: &serde_json::Value| -> Vec<String> {
        env["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["part_name"].as_str().unwrap().to_string())
            .collect()
    };

    // PRICE + ASC → C2(1.00) C3(5.00) C1(9.00)
    let (s, env) = get_sent_parts(&app, &token, company, "?sort_by=PRICE&sort_dir=ASC").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        draw_names(&env),
        vec!["NAME-C2", "NAME-C3", "NAME-C1"],
        "{env}"
    );

    // SENT_AT + DESC（默认方向）→ C1 C3 C2
    let (s, env) = get_sent_parts(&app, &token, company, "?sort_by=SENT_AT").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        draw_names(&env),
        vec!["NAME-C1", "NAME-C3", "NAME-C2"],
        "{env}"
    );

    // RECEIVED_AT + DESC → C2(09-20) C3(09-15) C1(09-10)
    let (s, env) =
        get_sent_parts(&app, &token, company, "?sort_by=RECEIVED_AT&sort_dir=DESC").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        draw_names(&env),
        vec!["NAME-C2", "NAME-C3", "NAME-C1"],
        "{env}"
    );
}

/// `drawing_no` **零命中**必须返回 0 行。
///
/// 2026-10-09 起 `drawing_no` 是直连 ILIKE 谓词
/// （`$2::text IS NULL OR p.drawing_no ILIKE $2`），零命中在 SQL 里就是零行 ——
/// 不再依赖 service 层那个「给了关键词却零命中要早返回」的分支（它是旧
/// `part_keyword_search` 预搜索的补偿逻辑：空数组让 `cardinality($2) = 0` 成立、
/// 整个条件被短路，从而返回该公司的**全部** shipment，list 与 count 同时错）。
#[tokio::test]
async fn sent_parts_drawing_no_zero_match_returns_empty_not_all_rows() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "ZeroCo", "Z").await;
    // 两台零件 / 两条 shipment，都不含 keyword 里那个词
    let p1 = insert_part(&pool, cid, "ONE").await;
    let p2 = insert_part(&pool, cid, "TWO").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "ZOPROC").await;
    let q1 = insert_quote(&pool, p1, company, proc_id).await;
    let q2 = insert_quote(&pool, p2, company, proc_id).await;
    insert_shipment(
        &pool,
        q1,
        p1,
        None,
        company,
        proc_id,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;
    insert_shipment(
        &pool,
        q2,
        p2,
        None,
        company,
        proc_id,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;

    // 不传筛选 → 全量 2 条（对照组：证明"全量"本身是可达的，不是断言写错）
    let (s, env) = get_sent_parts(&app, &token, company, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "不传筛选应返回全量: {env}");

    // 零命中 drawing_no → 0 条
    let (s, env) = get_sent_parts(&app, &token, company, "?drawing_no=NOSUCHTOKENZZ").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "零命中 drawing_no 必须 total=0（曾返回全量 2）: {env}"
    );
    assert!(
        env["data"]["items"].as_array().unwrap().is_empty(),
        "零命中 drawing_no 必须 items 为空: {env}"
    );

    // `name` 零命中同样为 0
    let (s, env) = get_sent_parts(&app, &token, company, "?name=NOSUCHTOKENZZ").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "零命中 name 必须 total=0: {env}");
}

/// 2026-10-03：`sort_by` / `sort_dir` 白名单**大小写不敏感** ——
/// `?sort_by=price` 与 `?sort_by=PRICE` 等价（大小写敏感时小写会静默回落成
/// `SENT_AT`：排序不生效且不报错）。
#[tokio::test]
async fn sent_parts_sort_whitelist_is_case_insensitive() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "CaseCo", "C").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "CSPROC").await;
    for (tag, price) in [("HI", "9.00"), ("LO", "1.00"), ("MID", "5.00")] {
        let pid = insert_part(&pool, cid, tag).await;
        let qid = insert_quote(&pool, pid, company, proc_id).await;
        insert_shipment(
            &pool,
            qid,
            pid,
            None,
            company,
            proc_id,
            1,
            price,
            "RECEIVED",
            "2026-09-01 10:00:00",
            Some("2026-09-02 10:00:00"),
        )
        .await;
    }

    let draw_names = |env: &serde_json::Value| -> Vec<String> {
        env["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["part_name"].as_str().unwrap().to_string())
            .collect()
    };

    // 小写 price + asc → LO(1.00) MID(5.00) HI(9.00)。
    // 大小写敏感时 `price` 回落 `SENT_AT DESC` → HI MID LO（sent_at 相同则按 id DESC，
    // 插入序逆序），断言必红。
    let (s, env) = get_sent_parts(&app, &token, company, "?sort_by=price&sort_dir=asc").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        draw_names(&env),
        vec!["NAME-LO", "NAME-MID", "NAME-HI"],
        "sort_by/sort_dir 必须大小写不敏感: {env}"
    );
}

/// 非法 `sort_by` / `sort_dir` 必须回落到 `SENT_AT` / `DESC` **并验行序**。
///
/// 1 行数据验不出序（只能证明"没注入、没报错、没删数据"），故种 3 行且让
/// `sent_at` / `received_at` / `unit_price` 三列互不同向 —— 任何回落列的错配
/// 都会改变行序。
#[tokio::test]
async fn sent_parts_illegal_sort_by_falls_back_to_sent_at_desc_without_injection() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "InjCo", "Y").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "IJPROC").await;
    // 3 行的 `sent_at` 升序、`received_at` **降序**、`unit_price` 与 `sent_at`
    // **不同向**（9 / 1 / 5）。这样 4 种可能的回落 / 误映射给出的行序两两不同：
    //   SENT_AT DESC（期望）  → LATE, MID, EARLY
    //   PRICE ASC             → MID, LATE, EARLY
    //   PRICE DESC            → EARLY, LATE, MID
    //   RECEIVED_AT DESC      → EARLY, MID, LATE
    // 只种 1 行时这 4 种全同，断言无法区分 —— 这是原用例的根本缺口。
    for (tag, price, sent, recv) in [
        (
            "EARLY",
            "9.00",
            "2026-09-01 10:00:00",
            "2026-09-20 10:00:00",
        ),
        ("MID", "1.00", "2026-09-02 10:00:00", "2026-09-15 10:00:00"),
        ("LATE", "5.00", "2026-09-03 10:00:00", "2026-09-10 10:00:00"),
    ] {
        let pid = insert_part(&pool, cid, tag).await;
        let qid = insert_quote(&pool, pid, company, proc_id).await;
        insert_shipment(
            &pool,
            qid,
            pid,
            None,
            company,
            proc_id,
            1,
            price,
            "RECEIVED",
            sent,
            Some(recv),
        )
        .await;
    }

    // 注入串 + 未知列名 + 非法方向：必须回落 sent_at / DESC，且不报错、表还在
    let (s, env) = get_sent_parts(
        &app,
        &token,
        company,
        "?sort_by=%27%3B%20DROP%20TABLE%20t_outsource_shipment%3B%20--&sort_dir=sideways",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 3, "{env}");
    // ★ 回落语义本体：3 行按 sent_at 降序。删掉这两行断言，本用例就退化成
    // "没注入、没报错"而不再验序。
    let names: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["part_name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["NAME-LATE", "NAME-MID", "NAME-EARLY"],
        "非法 sort_by / sort_dir 必须回落到 SENT_AT / DESC: {env}"
    );
    let still_there: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM t_outsource_shipment")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still_there, 3, "注入串不得影响 SQL");
}

#[tokio::test]
async fn sent_parts_pagination_total_limit_offset() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "PgCo", "Z").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "PGPROC").await;
    for i in 0..5 {
        let tag = format!("P{i}");
        let pid = insert_part(&pool, cid, &tag).await;
        let qid = insert_quote(&pool, pid, company, proc_id).await;
        insert_shipment(
            &pool,
            qid,
            pid,
            None,
            company,
            proc_id,
            1,
            "1.00",
            "OUTSOURCING",
            "2026-09-01 10:00:00",
            None,
        )
        .await;
    }

    let (s, env) = get_sent_parts(&app, &token, company, "?limit=2").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 5, "{env}");
    assert_eq!(env["data"]["limit"], 2, "{env}");
    assert_eq!(env["data"]["offset"], 0, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 2, "{env}");

    let (s, env) = get_sent_parts(&app, &token, company, "?limit=2&offset=4").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 5, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 1, "{env}");

    // offset 越界 → 空 items，total 仍 5
    let (s, env) = get_sent_parts(&app, &token, company, "?limit=2&offset=99").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 5, "{env}");
    assert!(env["data"]["items"].as_array().unwrap().is_empty(), "{env}");
}

// ===========================================================================
//  in-flight
// ===========================================================================

#[tokio::test]
async fn in_flight_only_returns_outsourcing_and_takes_batch_version_and_quantity() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "FlCo", "B").await;
    let p1 = insert_part(&pool, cid, "FLY").await;
    let p2 = insert_part(&pool, cid, "RCV").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "FLPROC").await;
    let q1 = insert_quote(&pool, p1, company, proc_id).await;
    let q2 = insert_quote(&pool, p2, company, proc_id).await;

    // 批次 version=7 / quantity=9，与 shipment 的 0 / 2 刻意不同 → 断言能区分
    let b1 = insert_batch(&pool, p1, 9, 7).await;
    let b2 = insert_batch(&pool, p2, 5, 3).await;
    insert_shipment(
        &pool,
        q1,
        p1,
        Some(b1),
        company,
        proc_id,
        2,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;
    // RECEIVED 的行不得出现在在途列表
    insert_shipment(
        &pool,
        q2,
        p2,
        Some(b2),
        company,
        proc_id,
        2,
        "1.00",
        "RECEIVED",
        "2026-09-02 10:00:00",
        Some("2026-09-03 10:00:00"),
    )
    .await;

    let (s, env) = send(
        app.clone(),
        json_request("GET", "/outsource-shipments/in-flight", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["part_id"], p1.to_string(), "{env}");
    assert_eq!(row["batch_id"], b1.to_string(), "{env}");
    // ★ version 取 t_part_batch.version（7），不是 shipment.version（0）
    assert_eq!(row["version"], 7, "version 必须取 batch.version: {env}");
    // ★ quantity 取 t_part_batch.quantity（9），不是 shipment.quantity（2）
    assert_eq!(row["quantity"], 9, "quantity 必须取 batch.quantity: {env}");
    assert_eq!(row["customer_path"], "FlCo", "{env}");
    assert_eq!(row["next_process_id"], proc_id.to_string(), "{env}");
    assert!(row["sent_at"].is_string(), "{env}");
}

#[tokio::test]
async fn in_flight_keyword_filter() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "FkCo", "D").await;
    let p1 = insert_part(&pool, cid, "HITME").await;
    let p2 = insert_part(&pool, cid, "NOPE").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "FKPROC").await;
    let q1 = insert_quote(&pool, p1, company, proc_id).await;
    let q2 = insert_quote(&pool, p2, company, proc_id).await;
    let b1 = insert_batch(&pool, p1, 1, 0).await;
    let b2 = insert_batch(&pool, p2, 1, 0).await;
    insert_shipment(
        &pool,
        q1,
        p1,
        Some(b1),
        company,
        proc_id,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;
    insert_shipment(
        &pool,
        q2,
        p2,
        Some(b2),
        company,
        proc_id,
        1,
        "1.00",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            "/outsource-shipments/in-flight?keyword=HITME",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert_eq!(env["data"]["items"][0]["part_id"], p1.to_string(), "{env}");
}

/// INSPECTOR 必须能读在途列表（2026-10-04 权限对齐）。
///
/// 外协三个写端点（`send-to-outsource` / `receive-from-outsource` /
/// `receive-from-outsource-to-inspection`）与菜单都已授予 INSPECTOR，读侧在途面
/// 若只放 Manager + Clerk，Inspector 就是「能发能收却看不到在途、点不到接收」。
/// 本端点纯只读且出参不含任何价格列，故一并放宽。
///
/// 对照断言（防止把守卫整段删掉）：`reconcile-update` 仍对 INSPECTOR 403。
#[tokio::test]
async fn in_flight_allows_inspector_while_reconcile_update_still_forbids() {
    let (pool, app, token) = bootstrap_as_inspector().await;
    let cid = insert_l1_customer(&pool, "FiCo", "H").await;
    let pid = insert_part(&pool, cid, "INSP").await;
    let company = OutsourceFixture::OUTSOURCE_COMPANY_ID;
    let proc_id = seed_outsource_process(&pool, "FIPROC").await;
    let qid = insert_quote(&pool, pid, company, proc_id).await;
    let bid = insert_batch(&pool, pid, 4, 5).await;
    let sid = insert_shipment(
        &pool,
        qid,
        pid,
        Some(bid),
        company,
        proc_id,
        1,
        "7.50",
        "OUTSOURCING",
        "2026-09-01 10:00:00",
        None,
    )
    .await;

    let (s, env) = send(
        app.clone(),
        json_request("GET", "/outsource-shipments/in-flight", None, Some(&token)),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "INSPECTOR 读在途必须 200（曾 403）: {env}"
    );
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["batch_id"], bid.to_string(), "{env}");
    assert_eq!(row["version"], 5, "version 必须取 batch.version: {env}");
    assert_eq!(row["quantity"], 4, "quantity 必须取 batch.quantity: {env}");
    // 出参不含任何价格列 —— 放宽权限的安全前提，改 VO 时要重新评估
    assert!(
        row.get("price").is_none(),
        "in-flight 行不得含 price: {env}"
    );
    assert!(
        row.get("unit_price").is_none(),
        "in-flight 行不得含 unit_price: {env}"
    );

    // 对照：对账写端点含 unit_price，权限维持 Manager + Clerk
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-shipments/{sid}/reconcile-update"),
            Some(json!({"version": 0, "quantity": 2})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "reconcile-update 必须仍对 INSPECTOR 403: {env}"
    );
    assert_eq!(env["code"], 40300, "{env}");
}
