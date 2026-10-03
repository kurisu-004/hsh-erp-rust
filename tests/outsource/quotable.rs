//! `GET /outsource-quotes/quotable-parts` 集成测试（2026-10-03 新增）
//!
//! 覆盖：
//! - happy path：货架绑了 OUTSOURCE 工序 + 批次在架上 → 出现
//! - 货架**没绑** OUTSOURCE 工序 → 不出现
//! - 该 OUTSOURCE 工序**不在**零件工艺链内 → 不出现（关键回归：少了这条筛选，
//!   `send-to-outsource` 的 `resolve_step_id_by_process` 会 404）
//! - 同一 (part, process) 有 2 个活跃批次 → **只出一行**（`DISTINCT ON`）
//! - `next_process_id` / `next_process_name` 确实返回（前端靠它自动填报价工序）
//! - 路由不被 `/{id}` 吞掉（未带 `/{id}` 路径也能通；响应是分页信封不是 400）

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

async fn insert_part(pool: &PgPool, customer_id: i64, tag: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, is_urgent, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 1, 33.00, 33.00, CURRENT_DATE, CURRENT_DATE, $4, false, \
                 'PENDING', 0, $5, $5)",
    )
    .bind(id)
    .bind(format!("NAME-{tag}"))
    .bind(format!("DWG-{tag}-{id}"))
    .bind(customer_id)
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

/// 建链 + 绑 part + 加 step（OUTSOURCE 工序必须出现在链内）。
async fn create_chain_with_step(pool: &PgPool, part_id: i64, process_id: i64) -> i64 {
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
    chain_id
}

/// 批次挂在货架上（`current_holder_id` = shelf_id）。
async fn insert_batch_on_shelf(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    shelf_id: i64,
    status: &str,
    location: Option<&str>,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_part_batch \
         (id, part_id, batch_no, quantity, status, location, current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 5, $4, $5, $6, 0, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(status)
    .bind(location)
    .bind(shelf_id)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

async fn get_quotable(
    app: &axum::Router,
    token: &str,
    qs: &str,
) -> (StatusCode, serde_json::Value) {
    send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-quotes/quotable-parts{qs}"),
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
async fn quotable_happy_path_returns_next_process_and_shelf() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtCo", "B").await;
    let pid = insert_part(&pool, cid, "HAPPY").await;
    let proc_id = seed_outsource_process(&pool, "QTHP").await;
    let shelf_id = insert_shelf(&pool, "C2A").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    create_chain_with_step(&pool, pid, proc_id).await;
    insert_batch_on_shelf(
        &pool,
        pid,
        1,
        shelf_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
    )
    .await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["id"], pid.to_string(), "{env}");
    // ★ next_process_id 是显式字段（前端靠它自动填报价工序，不做临时 cast）
    assert_eq!(row["next_process_id"], proc_id.to_string(), "{env}");
    assert_eq!(row["next_process_name"], "PROC-QTHP", "{env}");
    assert_eq!(row["shelf_id"], shelf_id.to_string(), "{env}");
    assert_eq!(row["shelf_code"], "C2A", "{env}");
    assert_eq!(row["unit_price"], "33.00", "{env}");
    // 客户路径：本例 customer 是根 L1（无 parent）→ 只给自身名
    assert_eq!(row["customer_path"], "QtCo", "{env}");
    assert_eq!(row["l1_customer_name"], serde_json::Value::Null, "{env}");
}

#[tokio::test]
async fn quotable_shelf_without_outsource_process_excluded() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtNoProc", "D").await;
    let pid = insert_part(&pool, cid, "NOPROC").await;
    // 另一台货架只绑了「非外协」工序 → 不应命中
    let other_proc = {
        let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
             version, created_at, updated_at) VALUES ($1, 'INH-QT', 'INH', 'INHOUSE', 0, false, 0, $2, $2)",
        )
        .bind(id)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert INHOUSE process");
        id
    };
    let shelf_id = insert_shelf(&pool, "C2B").await;
    link_shelf_process(&pool, shelf_id, other_proc).await;
    create_chain_with_step(&pool, pid, other_proc).await;
    insert_batch_on_shelf(
        &pool,
        pid,
        1,
        shelf_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
    )
    .await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "货架未绑 OUTSOURCE 工序必须不出现: {env}"
    );
}

#[tokio::test]
async fn quotable_process_not_in_part_chain_excluded() {
    // ★ 关键回归：货架绑了 OUTSOURCE 工序，但该工序不在零件工艺链内 → 必须不出现
    // （少了这条筛选，后续 send-to-outsource 的 resolve_step_id_by_process 会 404）
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtNoChain", "E").await;
    let pid = insert_part(&pool, cid, "NOCHAIN").await;
    let chain_proc = seed_outsource_process(&pool, "QTC-IN").await;
    let shelf_proc = seed_outsource_process(&pool, "QTC-OUT").await;
    let shelf_id = insert_shelf(&pool, "C2C").await;
    // 货架只绑 shelf_proc；工艺链里只有 chain_proc
    link_shelf_process(&pool, shelf_id, shelf_proc).await;
    create_chain_with_step(&pool, pid, chain_proc).await;
    insert_batch_on_shelf(
        &pool,
        pid,
        1,
        shelf_id,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
    )
    .await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "OUTSOURCE 工序不在零件工艺链内必须不出现: {env}"
    );
}

#[tokio::test]
async fn quotable_dedups_multiple_batches_same_part_process() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtDup", "G").await;
    let pid = insert_part(&pool, cid, "DUP").await;
    let proc_id = seed_outsource_process(&pool, "QTDUP").await;
    let shelf_id = insert_shelf(&pool, "C2D").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    create_chain_with_step(&pool, pid, proc_id).await;
    // 同一 (part, process) 两个活跃批次
    insert_batch_on_shelf(&pool, pid, 1, shelf_id, "PENDING", None).await;
    insert_batch_on_shelf(&pool, pid, 2, shelf_id, "PENDING", None).await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 1,
        "同一 (part, process) 多批次只出一行: {env}"
    );
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 1, "{env}");
}

#[tokio::test]
async fn quotable_pending_batch_without_holder_excluded() {
    // PENDING 且没上架（current_holder_id IS NULL）→ 命中不了货架工序 → 不出现
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtNoShelf", "H").await;
    let pid = insert_part(&pool, cid, "NOSHELF").await;
    let proc_id = seed_outsource_process(&pool, "QTNOS").await;
    let shelf_id = insert_shelf(&pool, "C2E").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    create_chain_with_step(&pool, pid, proc_id).await;
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 5, 'PENDING', 0, now(), now())",
    )
    .bind(id)
    .bind(pid)
    .execute(&pool)
    .await
    .expect("insert t_part_batch");

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "未上架的 PENDING 批次不应出现: {env}"
    );
}

#[tokio::test]
async fn quotable_keyword_filter_and_pagination() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtKw", "I").await;
    let proc_id = seed_outsource_process(&pool, "QTKW").await;
    let shelf_id = insert_shelf(&pool, "C2F").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    for tag in ["KWA", "KWB"] {
        let pid = insert_part(&pool, cid, tag).await;
        create_chain_with_step(&pool, pid, proc_id).await;
        insert_batch_on_shelf(&pool, pid, 1, shelf_id, "PENDING", None).await;
    }

    let (s, env) = get_quotable(&app, &token, "?keyword=KWA").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert!(
        env["data"]["items"][0]["drawing_no"]
            .as_str()
            .unwrap()
            .contains("KWA"),
        "{env}"
    );

    let (s, env) = get_quotable(&app, &token, "?limit=1&offset=1").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 1, "{env}");
    assert_eq!(env["data"]["offset"], 1, "{env}");
}

#[tokio::test]
async fn quotable_route_not_swallowed_by_id_path_param() {
    // 回归：请求不得落到 `/{id}`（Path<i64>）上被 PathRejection 打成 400
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = get_quotable(&app, &token, "?limit=5").await;
    assert_eq!(
        s,
        StatusCode::OK,
        "quotable-parts 被 /{{id}} 吞掉会返 400: {env}"
    );
    assert_eq!(env["code"], 0, "{env}");
    assert!(env["data"]["items"].is_array(), "{env}");
    assert!(env["data"]["total"].is_i64(), "{env}");
}
