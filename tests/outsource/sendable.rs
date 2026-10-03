//! `GET /outsource-sendable` 集成测试（2026-10-03 新增）
//!
//! 覆盖：
//! - APPROVAL 模式：有 APPROVED 报价 → `send_mode="APPROVAL"` / `price` 非 null /
//!   `quote_id` 非 null / `company_options` 空数组
//! - DIRECT 模式：无 APPROVED 报价但货架工序映射了活跃公司 → `company_options`
//!   正确列出公司 id + name
//! - DIRECT 但未映射任何活跃公司 → **该行仍返回**，`company_options` 空数组
//! - `version` == `t_part_batch.version`；`source_status` 区分 PENDING / IN_PROCESS
//! - `customer_id` query 过滤生效
//! - `total` 与 items 实际行数一致（含 DIRECT 空 options 行）
//! - `is_urgent` 排序在首

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, send, test_app, test_pool,
    test_state,
};

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

async fn insert_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
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

async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    tag: &str,
    urgent: bool,
    planned: &str,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, is_urgent, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 5, 1.00, 5.00, CURRENT_DATE, $4::date, $5, $6, 'PENDING', 0, $7, $7)",
    )
    .bind(id)
    .bind(format!("NAME-{tag}"))
    .bind(format!("DWG-{tag}-{id}"))
    .bind(planned)
    .bind(customer_id)
    .bind(urgent)
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
    .bind(format!("PROC-{code}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

async fn insert_shelf(pool: &PgPool, code: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'PRODUCTION', true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(format!("shelf-{code}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

async fn link_shelf_process(pool: &PgPool, shelf_id: i64, process_id: i64) {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, now(), now())",
    )
    .bind(id)
    .bind(shelf_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
}

async fn create_chain_with_step(pool: &PgPool, part_id: i64, process_id: i64) {
    let chain_id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 0, now(), 0, now(), 0)",
    )
    .bind(chain_id)
    .bind(format!("chain-{part_id}"))
    .execute(pool)
    .await
    .expect("insert chain");
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("bind part to chain");
    let step_id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, estimated_minutes, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 30, 0, now(), 0, now(), 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert step");
}

async fn insert_batch_on_shelf(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    shelf_id: Option<i64>,
    status: &str,
    location: Option<&str>,
    version: i32,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_part_batch \
         (id, part_id, batch_no, quantity, status, location, current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 8, $4, $5, $6, $7, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(status)
    .bind(location)
    .bind(shelf_id)
    .bind(version)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

async fn insert_company(pool: &PgPool, name: &str, is_active: bool) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company (id, name, is_active, version, created_at, updated_at) \
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

async fn link_company_process(pool: &PgPool, company_id: i64, process_id: i64) {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_company_process \
         (id, outsource_company_id, process_id, sort_order, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, now(), now())",
    )
    .bind(id)
    .bind(company_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert t_outsource_company_process");
}

async fn insert_approved_quote(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
    price: &str,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_quote \
         (id, part_id, outsource_company_id, process_id, price, status, submitted_at, reviewed_at, \
          version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5::numeric, 'APPROVED', now(), now(), 0, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(company_id)
    .bind(process_id)
    .bind(price)
    .execute(pool)
    .await
    .expect("insert t_outsource_quote");
    id
}

async fn get_sendable(
    app: &axum::Router,
    token: &str,
    qs: &str,
) -> (StatusCode, serde_json::Value) {
    send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-sendable{qs}"),
            None,
            Some(token),
        ),
    )
    .await
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn sendable_approval_mode_when_approved_quote_exists() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdCo", "J").await;
    let pid = insert_part(&pool, cid, "APPR", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDAP").await;
    let shelf_id = insert_shelf(&pool, "SA1").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    // 同样映射一家公司 —— APPROVAL 模式下 company_options 必须仍为空数组
    let co = insert_company(&pool, "ApprovalCo", true).await;
    link_company_process(&pool, co, proc_id).await;
    create_chain_with_step(&pool, pid, proc_id).await;
    let bid = insert_batch_on_shelf(&pool, pid, 1, Some(shelf_id), "PENDING", None, 4).await;
    let qid = insert_approved_quote(&pool, pid, co, proc_id, "12.50").await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["send_mode"], "APPROVAL", "{env}");
    assert_eq!(row["price"], "12.50", "{env}");
    assert_eq!(row["quote_id"], qid.to_string(), "{env}");
    assert_eq!(row["outsource_company_id"], co.to_string(), "{env}");
    assert_eq!(row["outsource_company_name"], "ApprovalCo", "{env}");
    assert_eq!(
        row["company_options"].as_array().unwrap().len(),
        0,
        "APPROVAL 模式 company_options 必须空数组: {env}"
    );
    // version 取 batch.version（4），batch_quantity / quantity 都来自批次
    assert_eq!(row["version"], 4, "{env}");
    assert_eq!(row["batch_id"], bid.to_string(), "{env}");
    assert_eq!(row["batch_no"], 1, "{env}");
    assert_eq!(row["batch_quantity"], 8, "{env}");
    assert_eq!(row["quantity"], 8, "{env}");
    assert_eq!(row["source_status"], "PENDING", "{env}");
    assert_eq!(row["next_process_id"], proc_id.to_string(), "{env}");
    assert_eq!(row["shelf_code"], "SA1", "{env}");
    assert_eq!(row["status_label"], "sendable", "{env}");
    assert_eq!(row["customer_path"], "SdCo", "{env}");
    assert_eq!(row["planned_delivery_date"], "2026-12-01", "{env}");
}

#[tokio::test]
async fn sendable_direct_mode_lists_active_company_options() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdDir", "K").await;
    let pid = insert_part(&pool, cid, "DIR", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDDR").await;
    let shelf_id = insert_shelf(&pool, "SB1").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    let co1 = insert_company(&pool, "DirectCo1", true).await;
    let co2 = insert_company(&pool, "DirectCo2", true).await;
    // 停用公司不得进 company_options
    let co_off = insert_company(&pool, "DisabledCo", false).await;
    link_company_process(&pool, co1, proc_id).await;
    link_company_process(&pool, co2, proc_id).await;
    link_company_process(&pool, co_off, proc_id).await;
    create_chain_with_step(&pool, pid, proc_id).await;
    insert_batch_on_shelf(&pool, pid, 1, Some(shelf_id), "PENDING", None, 0).await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["send_mode"], "DIRECT", "{env}");
    // DIRECT 无报价
    assert!(row["quote_id"].is_null(), "{env}");
    assert!(row["price"].is_null(), "{env}");
    assert!(row["outsource_company_id"].is_null(), "{env}");
    let opts = row["company_options"].as_array().unwrap();
    assert_eq!(opts.len(), 2, "只列活跃公司: {env}");
    let ids: Vec<String> = opts
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&co1.to_string()), "{env}");
    assert!(ids.contains(&co2.to_string()), "{env}");
    assert!(
        !ids.contains(&co_off.to_string()),
        "停用公司不得出现: {env}"
    );
    let names: Vec<&str> = opts.iter().map(|o| o["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"DirectCo1"), "{env}");
    assert!(names.contains(&"DirectCo2"), "{env}");
}

#[tokio::test]
async fn sendable_direct_row_kept_when_no_active_company() {
    // DIRECT 且该工序没映射任何活跃公司 → 行仍返回（前端 canSend() 置灰），
    // 但 total 也要把它算进去，否则分页对不上
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdBare", "L").await;
    let pid = insert_part(&pool, cid, "BARE", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDBR").await;
    let shelf_id = insert_shelf(&pool, "SC1").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    create_chain_with_step(&pool, pid, proc_id).await;
    insert_batch_on_shelf(&pool, pid, 1, Some(shelf_id), "PENDING", None, 0).await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 1,
        "空 options 行也必须计入 total: {env}"
    );
    let row = &env["data"]["items"][0];
    assert_eq!(row["send_mode"], "DIRECT", "{env}");
    assert_eq!(row["company_options"].as_array().unwrap().len(), 0, "{env}");
}

#[tokio::test]
async fn sendable_source_status_and_batch_version() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdSt", "M").await;
    let proc_id = seed_outsource_process(&pool, "SDST").await;
    let shelf_id = insert_shelf(&pool, "SD1").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;

    // PENDING
    let p_pending = insert_part(&pool, cid, "PEND", false, "2026-12-01").await;
    create_chain_with_step(&pool, p_pending, proc_id).await;
    let b_pending =
        insert_batch_on_shelf(&pool, p_pending, 1, Some(shelf_id), "PENDING", None, 11).await;

    // IN_PROCESS + PRODUCTION_SHELF
    let p_inproc = insert_part(&pool, cid, "INPC", false, "2026-12-02").await;
    create_chain_with_step(&pool, p_inproc, proc_id).await;
    let b_inproc = insert_batch_on_shelf(
        &pool,
        p_inproc,
        1,
        Some(shelf_id),
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        22,
    )
    .await;

    // IN_PROCESS 但 location=WORKER → 不该出现
    let p_worker = insert_part(&pool, cid, "WRKR", false, "2026-12-03").await;
    create_chain_with_step(&pool, p_worker, proc_id).await;
    insert_batch_on_shelf(
        &pool,
        p_worker,
        1,
        Some(shelf_id),
        "IN_PROCESS",
        Some("WORKER"),
        33,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "WORKER 上的批次不应出现: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    // 雪花主键在 JSON 里是字符串，比较前先转字符串键
    let by_batch = |b: i64| {
        let key = b.to_string();
        items
            .iter()
            .find(|i| i["batch_id"].as_str() == Some(key.as_str()))
            .unwrap_or_else(|| panic!("missing batch {b}: {env}"))
    };
    let row_p = by_batch(b_pending);
    assert_eq!(row_p["source_status"], "PENDING", "{env}");
    assert_eq!(row_p["version"], 11, "version 必须取 batch.version: {env}");
    let row_i = by_batch(b_inproc);
    assert_eq!(row_i["source_status"], "IN_PROCESS", "{env}");
    assert_eq!(row_i["version"], 22, "version 必须取 batch.version: {env}");
}

#[tokio::test]
async fn sendable_customer_id_filter_and_keyword() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid_a = insert_customer(&pool, "CustA", "N").await;
    let cid_b = insert_customer(&pool, "CustB", "O").await;
    let proc_id = seed_outsource_process(&pool, "SDCF").await;
    let shelf_id = insert_shelf(&pool, "SE1").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    for (cid, tag) in [(cid_a, "FJA"), (cid_b, "FJB")] {
        let pid = insert_part(&pool, cid, tag, false, "2026-12-01").await;
        create_chain_with_step(&pool, pid, proc_id).await;
        insert_batch_on_shelf(&pool, pid, 1, Some(shelf_id), "PENDING", None, 0).await;
    }

    let (s, env) = get_sendable(&app, &token, &format!("?customer_id={cid_a}")).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert!(
        env["data"]["items"][0]["part_drawing_no"]
            .as_str()
            .unwrap()
            .contains("FJA"),
        "{env}"
    );

    let (s, env) = get_sendable(&app, &token, "?keyword=FJB").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
}

#[tokio::test]
async fn sendable_total_matches_items_and_pagination() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdPg", "Q").await;
    let proc_id = seed_outsource_process(&pool, "SDPG").await;
    let shelf_id = insert_shelf(&pool, "SF1").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    // 3 个 part：1 个 APPROVAL + 2 个 DIRECT（含 1 个无公司）
    let co = insert_company(&pool, "PgCo", true).await;
    link_company_process(&pool, co, proc_id).await;
    for i in 0..3 {
        let tag = format!("PG{i}");
        let pid = insert_part(&pool, cid, &tag, false, "2026-12-01").await;
        create_chain_with_step(&pool, pid, proc_id).await;
        insert_batch_on_shelf(&pool, pid, 1, Some(shelf_id), "PENDING", None, 0).await;
        if i == 0 {
            insert_approved_quote(&pool, pid, co, proc_id, "9.99").await;
        }
    }

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 3, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 3, "{env}");
    let modes: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["send_mode"].as_str().unwrap())
        .collect();
    assert_eq!(
        modes.iter().filter(|m| **m == "APPROVAL").count(),
        1,
        "{env}"
    );
    assert_eq!(modes.iter().filter(|m| **m == "DIRECT").count(), 2, "{env}");

    let (s, env) = get_sendable(&app, &token, "?limit=2&offset=2").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 3, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 1, "{env}");
}

#[tokio::test]
async fn sendable_orders_urgent_first_then_planned_delivery() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdOr", "R").await;
    let proc_id = seed_outsource_process(&pool, "SDOR").await;
    let shelf_id = insert_shelf(&pool, "SG1").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    // 顺序插入：普通-晚、普通-早、加急-晚 → 期望输出：加急-晚、普通-早、普通-晚
    let a = insert_part(&pool, cid, "NLA", false, "2026-12-30").await;
    let b = insert_part(&pool, cid, "NEB", false, "2026-12-10").await;
    let c = insert_part(&pool, cid, "URG", true, "2026-12-31").await;
    for pid in [a, b, c] {
        create_chain_with_step(&pool, pid, proc_id).await;
        insert_batch_on_shelf(&pool, pid, 1, Some(shelf_id), "PENDING", None, 0).await;
    }

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let names: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["part_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["NAME-URG", "NAME-NEB", "NAME-NLA"], "{env}");
    assert_eq!(env["data"]["items"][0]["is_urgent"], true, "{env}");
}

#[tokio::test]
async fn sendable_excludes_process_not_in_part_chain() {
    // 与 quotable-parts 同理：货架绑了 OUTSOURCE 工序但零件工艺链内没有 → 不出现
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdNc", "S").await;
    let pid = insert_part(&pool, cid, "NOCH", false, "2026-12-01").await;
    let chain_proc = seed_outsource_process(&pool, "SDNC-IN").await;
    let shelf_proc = seed_outsource_process(&pool, "SDNC-OUT").await;
    let shelf_id = insert_shelf(&pool, "SH1").await;
    link_shelf_process(&pool, shelf_id, shelf_proc).await;
    create_chain_with_step(&pool, pid, chain_proc).await;
    insert_batch_on_shelf(&pool, pid, 1, Some(shelf_id), "PENDING", None, 0).await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "{env}");
}

#[tokio::test]
async fn sendable_one_row_per_batch_process_even_with_many_batches() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdDup", "T").await;
    let pid = insert_part(&pool, cid, "DUPB", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDDUP").await;
    let shelf_id = insert_shelf(&pool, "SI1").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    create_chain_with_step(&pool, pid, proc_id).await;
    insert_batch_on_shelf(&pool, pid, 1, Some(shelf_id), "PENDING", None, 0).await;
    insert_batch_on_shelf(&pool, pid, 2, Some(shelf_id), "PENDING", None, 0).await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 2,
        "两个批次是**两行**（行粒度是 批次×工序）: {env}"
    );
    let batch_ids: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["batch_id"].as_str().unwrap())
        .collect();
    assert_eq!(batch_ids.len(), 2, "{env}");
    assert_ne!(batch_ids[0], batch_ids[1], "两行必须是不同批次: {env}");
}
