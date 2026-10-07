//! `GET /outsource-quotes/quotable-parts` 集成测试（2026-10-03 新增）
//!
//! 覆盖（行粒度 = 一零件一行，2026-10-03 由「零件 × OUTSOURCE 工序」简化而来）：
//! - happy path：零件有 PENDING 批次 → 出现
//! - 只有 IN_PROCESS 批次 → **不出现**（报价是给还没下发的零件准备的）
//! - 零件**没有**工艺链 → 仍然出现（picker 不看工艺链）
//! - 同一零件 2 个 PENDING 批次 → **只出一行**（`DISTINCT ON`）
//! - 未上架（无 holder）的 PENDING 批次 → 出现（该端点不再输出货架字段）
//! - 已软删零件 / 软删批次 → 不出现
//! - keyword + 分页
//! - 路由不被 `/{id}` 吞掉（未带 `/{id}` 路径也能通；响应是分页信封不是 400）

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, pool_snowflake, send,
    test_app, test_pool, test_state,
};

/// 直插用的雪花 ID：走 `test-support::pool_snowflake()`（**进程级**
/// `OnceLock<Mutex<..>>`，instance 由 pid ⊕ 启动时间派生）。
///
/// 不每次 `SnowflakeIdGenerator::new(epoch, 1)` 新建生成器：新建的生成器在同一毫秒内
/// 连续两次调用会生成**完全相同**的 id（instance 相同 + 时间戳相同 + seq 都从 0 开始），
/// 撞 `t_*_pkey`；更隐蔽的是撞成「shelf_id == process_id」这类业务列，让 DB 的
/// `ck_*_no_self_loop` CHECK 以一条与被测逻辑无关的约束错误把用例打断。范本与理由见
/// `tests/outsource/pool.rs::next_id`。
fn next_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
}

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
    let id = next_id();
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
    let id = next_id();
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

/// 直插一个批次。`holder` 可为 `None`（未上架的常态），`status` 由用例指定
/// （picker 只认 `PENDING`）。
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    status: &str,
    location: Option<&str>,
    holder: Option<i64>,
) -> i64 {
    let id = next_id();
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
    .bind(holder)
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
async fn quotable_happy_path_returns_part_fields() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtCo", "B").await;
    let pid = insert_part(&pool, cid, "HAPPY").await;
    insert_batch(&pool, pid, 1, "PENDING", None, None).await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["id"], pid.to_string(), "{env}");
    assert_eq!(row["unit_price"], "33.00", "{env}");
    // 客户路径：本例 customer 是根 L1（无 parent）→ 只给自身名
    assert_eq!(row["customer_path"], "QtCo", "{env}");
    assert_eq!(row["l1_customer_name"], serde_json::Value::Null, "{env}");
    // 2026-10-03：行粒度收成「一零件一行」，货架 / 工序四字段已从契约里删除
    for gone in [
        "shelf_id",
        "shelf_code",
        "next_process_id",
        "next_process_name",
    ] {
        assert!(
            row.get(gone).is_none(),
            "字段 {gone} 应已从 QuotablePartOut 移除: {row}"
        );
    }
}

/// picker 只给「还没下发」的零件报价：只有 IN_PROCESS 批次的不出现。
#[tokio::test]
async fn quotable_excludes_part_without_pending_batch() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtInProc", "D").await;
    let pid = insert_part(&pool, cid, "INPROC").await;
    insert_batch(
        &pool,
        pid,
        1,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(9_000_000_000_000_001_001),
    )
    .await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "已下发（在产）的零件不属于报价 picker: {env}"
    );
}

/// 2026-10-03：picker 不看工艺链 —— 零件完全没有 `process_chain_id` 也照常出现。
///
/// 旧谓词要求「候选外协工序在零件工艺链内」，而生产库里绝大多数零件没有链，
/// 叠加「按货架枚举外协工序」后 picker 长期恒空。
#[tokio::test]
async fn quotable_includes_part_without_process_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtNoChain", "E").await;
    let pid = insert_part(&pool, cid, "NOCHAIN").await;
    // 前提断言：part 确实没有链
    let chain: Option<i64> =
        sqlx::query_scalar("SELECT process_chain_id FROM t_part WHERE id = $1")
            .bind(pid)
            .fetch_one(&pool)
            .await
            .expect("read process_chain_id");
    assert!(chain.is_none(), "本用例前提是 part 无工艺链");
    insert_batch(&pool, pid, 1, "PENDING", None, None).await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "无链零件也必须进 picker: {env}");
    assert_eq!(env["data"]["items"][0]["id"], pid.to_string(), "{env}");
}

/// 同一零件多个 PENDING 批次 → 只出一行（`DISTINCT ON (p.id)`）。
#[tokio::test]
async fn quotable_dedups_multiple_pending_batches_same_part() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtDup", "G").await;
    let pid = insert_part(&pool, cid, "DUP").await;
    insert_batch(&pool, pid, 1, "PENDING", None, None).await;
    insert_batch(&pool, pid, 2, "PENDING", None, None).await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 1,
        "同一零件多个 PENDING 批次只出一行: {env}"
    );
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 1, "{env}");
}

/// `PENDING` + `IN_PROCESS` 混合：只要有一个 PENDING 批次就出行。
#[tokio::test]
async fn quotable_includes_part_with_pending_and_in_process_batches() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtMix", "H").await;
    let pid = insert_part(&pool, cid, "MIXED").await;
    insert_batch(
        &pool,
        pid,
        1,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(9_000_000_000_000_001_002),
    )
    .await;
    insert_batch(&pool, pid, 2, "PENDING", None, None).await;

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert_eq!(env["data"]["items"][0]["id"], pid.to_string(), "{env}");
}

/// 软删边界：软删零件 / 软删 PENDING 批次都不出行。
#[tokio::test]
async fn quotable_excludes_soft_deleted_part_and_batch() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtSoft", "I").await;

    let p_soft = insert_part(&pool, cid, "SOFTP").await;
    insert_batch(&pool, p_soft, 1, "PENDING", None, None).await;
    sqlx::query("UPDATE t_part SET deleted_at = now() WHERE id = $1")
        .bind(p_soft)
        .execute(&pool)
        .await
        .expect("soft delete part");

    let p_bsoft = insert_part(&pool, cid, "SOFTB").await;
    let b = insert_batch(&pool, p_bsoft, 1, "PENDING", None, None).await;
    sqlx::query("UPDATE t_part_batch SET deleted_at = now() WHERE id = $1")
        .bind(b)
        .execute(&pool)
        .await
        .expect("soft delete batch");

    let (s, env) = get_quotable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "软删零件 / 软删批次都不应出现: {env}"
    );
}

#[tokio::test]
async fn quotable_keyword_filter_and_pagination() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_l1_customer(&pool, "QtKw", "J").await;
    for tag in ["KWA", "KWB"] {
        let pid = insert_part(&pool, cid, tag).await;
        insert_batch(&pool, pid, 1, "PENDING", None, None).await;
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
