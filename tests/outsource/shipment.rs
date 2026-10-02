//! outsource 读侧 shipment list 集成测试（2026-10-03 新增）
//!
//! 覆盖：
//! - `GET /outsource-companies/{id}/sent-parts`：happy path（OUTSOURCING + RECEIVED
//!   都出现）/ keyword 过滤（命中 + **零命中返回 0 行**）/ sent_at 日期窗 /
//!   3 种 sort_by / sort 白名单大小写不敏感 / 非法 sort_by 回落并**验序** /
//!   分页 total+offset
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
    OutsourceFixture, json_request, load_outsource_fixture, login_token, send, test_app, test_pool,
    test_state,
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

/// 直插 part；`drawing_tag` 便于 keyword 断言区分。
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
}

#[tokio::test]
async fn sent_parts_keyword_filter_hits_only_matching_part() {
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

    let (s, env) = get_sent_parts(&app, &token, company, "?keyword=MATCHME").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert_eq!(env["data"]["items"][0]["part_id"], hit.to_string(), "{env}");
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
    assert_eq!(env["data"]["items"][0]["part_id"], p1.to_string(), "{env}");
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

/// 2026-10-03 review 第 1 轮 BLOCKER-1：给了 keyword 却**零命中**时必须返回 0 行。
///
/// 回归的 bug：SQL 谓词是
/// `AND (cardinality($2::bigint[]) = 0 OR s.part_id = ANY($2))`，
/// `part_keyword_search` 零命中时给出空数组 → `cardinality = 0` 成立 →
/// 整个 keyword 条件被短路掉 → 返回该公司的**全部** shipment（list 与 count
/// 同时错，`total` 也一起错）。
#[tokio::test]
async fn sent_parts_keyword_zero_match_returns_empty_not_all_rows() {
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

    // 无 keyword → 全量 2 条（对照组：证明"全量"本身是可达的，不是断言写错）
    let (s, env) = get_sent_parts(&app, &token, company, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "无 keyword 应返回全量: {env}");

    // 零命中 keyword → 0 条。**空 items 是本用例的全部断言**，删掉 service 层的
    // 早返回就会拿到 2 条 → 红。
    let (s, env) = get_sent_parts(&app, &token, company, "?keyword=NOSUCHTOKENZZ").await;
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

/// 2026-10-03 review 第 1 轮 m5：`sort_by` / `sort_dir` 白名单**大小写不敏感**
/// （此前 `?sort_by=price` 静默回落成 `SENT_AT`，前端小写传参时排序不生效且不报错）。
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

/// 2026-10-03 review 第 1 轮 m1：补 2 条不同 `sent_at` 的行并**断言降序**。
///
/// 原用例只种 1 行 —— 1 行无法验序，只能证明"回落没注入、没报错、没删数据"，
/// 不能证明真的回落到 `SENT_AT` / `DESC`。
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
    // "没注入、没报错"而不再验序（review 第 1 轮 m1 点的正是这个缺口）。
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
