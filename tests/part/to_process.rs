//! part 域集成测试 —— to-process 流
//!
//! 覆盖：
//!   - to-process：shelf_id / next_process_id 非数字拒绝（FAIL 分支参数 → 新必填字段）
//!   - to-process：INSPECTION happy path / 非 INSPECTION 状态拒绝
//!   - to-process partial-split：INSPECTION 批次 qty=10 → quantity=3，拆批
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//! 每个用例 INSPECTOR token（白名单）。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::{json_request, load_part_fixture, login_token, send, test_app,
    test_pool, test_state};

// ===========================================================================
//  动态 fixture helpers（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::{create_chain_for_part / create_step}`
//  引入 2 helper，因 fixtures.rs 本轮被删，复制到本地（同形 sqlx::query 直插）。
// ===========================================================================

/// 为指定 part 建一个最小工艺链（t_part_process_chain），并把 part.process_chain_id 绑回。
async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let chain_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) \
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
    chain_id
}

/// 在指定 chain 内创建 step（process_id + sort_order）。
async fn create_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let step_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 30, 0, now(), 0, now(), 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(sort_order)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert chain step");
    step_id
}

// ===========================================================================
//  动态 part/batch 插入 helper（tests/part/ 各 sub-file 私有，PR-C 末统一迁）
// ===========================================================================

/// 创建一个 part（指定 status）和一个 batch（默认 INSPECTION，qty=5），
/// 返回 `(part_id, batch_id)`。
async fn insert_part_with_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    status: &str,
    batch_status: &str,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let batch_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-001', $4, $7, $3, $5, $5, 1, 0, $6, $6)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, 5, $3, 0, $4, $4)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(batch_status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    (part_id, batch_id)
}

async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
}

// ===========================================================================
//  bootstrap helpers
// ===========================================================================

async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Tests
// ===========================================================================

/// to-process 拒绝：shelf_id 非数字 → 20104 BIZ_INVALID_VALUE。
///
/// to-XXX 重命名：原一键送检的 FAIL 分支 `shelf_id` / `next_process_id` 在
/// `to-process` 成为必填字段（`ToProcessRequest.shelf_id: String`）。缺字段走
/// axum Json 提取 → 422 不在信封内（已退化为非 envelope plain text），故改用
/// 「非数字」值（"abc"）保留 service 层 20104 校验路径的覆盖。
#[tokio::test]
async fn to_process_invalid_shelf_id_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        "INSPECTION",
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": "abc",
                "next_process_id": "1",
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20104);
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains("abc"), "message 应含原值 'abc': {msg}");
}

/// to-process 拒绝：next_process_id 非数字 → 20104 BIZ_INVALID_VALUE。
#[tokio::test]
async fn to_process_invalid_next_process_id_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        "INSPECTION",
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": "abc",
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20104);
}

/// to-process happy path：INSPECTION → IN_PROCESS（推荐需求 3）。
///
/// 2026-09-16 PR-3 适配：to-process 入口要求 part 已绑定工艺链（20706）。
#[tokio::test]
async fn to_process_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        "INSPECTION",
    )
    .await;
    // PR-3：建链并把 part 绑到 chain
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": fx.process_id.to_string(),
                "note": "test fail",
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "IN_PROCESS");
}

/// to-process 拒绝：非 INSPECTION 状态（PENDING）→ 404 / 20109。
///
/// 2026-09-16 PR-3 后，state machine 允许 PENDING → IN_PROCESS（place-on-shelf 路径）；
/// to_process 是品检打回流，要求 part 已绑定工艺链 + 存在 INSPECTION 批次。
/// PENDING part 没有 INSPECTION 批次 → service 在 step 4 `find_inspection_batch_for_fail`
/// 抛 20109 BIZ_PART_BATCH_NOT_FOUND（HTTP 404）。
#[tokio::test]
async fn to_process_wrong_state_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // setup: PENDING part（非 INSPECTION）
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "PENDING",
        "PENDING",
    )
    .await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": fx.process_id.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body={body}");
    assert_eq!(body["code"], 20109, "BIZ_PART_BATCH_NOT_FOUND: {body}");
}

/// to-process partial-split happy path：INSPECTION 批次 qty=10 → quantity=3 → 拆批。
///
/// 期望：
/// - `new_batch_id` 非 null（remainder 留 INSPECTION）；
/// - `part.status` 保持 INSPECTION（rollup 守卫：剩 7 件仍在 INSPECTION，
///   部分打回整 part 不翻状态）。
/// - 响应 `part` 投影展示最新 OCC 版本。
#[tokio::test]
async fn to_process_partial_split_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let (part_id, batch_id) = insert_part_with_batch_qty(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "INSPECTION",
        "INSPECTION",
        10,
    )
    .await;
    // 2026-09-16 PR-3：to_process 要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": fx.process_id.to_string(),
                "quantity": 3,
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    // PR-B2 §5 改造：partial-split 后 part.status 走 batch rollup（min-progress）：
    //   operated 子批（qty=3）→ IN_PROCESS，remainder 子批（qty=7）留 INSPECTION
    //   → 物化 status = min(IN_PROCESS, INSPECTION) = IN_PROCESS。
    //   旧直接 UPDATE 路径会让 part.status = IN_PROCESS（operated 的状态）；
    //   新 rollup 路径结果一致（min-progress 也取 IN_PROCESS）。
    assert_eq!(
        body["data"]["part"]["status"], "IN_PROCESS",
        "partial-split 后 part.status 由 rollup 派生为 IN_PROCESS（min progress）"
    );
    // 拆批后剩余批次 id（remainder 留在 INSPECTION 待后续操作）
    let new_bid_str = body["data"]["new_batch_id"]
        .as_str()
        .expect("new_batch_id 应为 string (Some)");
    assert_eq!(
        new_bid_str,
        batch_id.to_string(),
        "remainder id 应回填为源批次 id"
    );
}

// ===========================================================================
//  内部 helper（qty=10 版本）
// ===========================================================================

async fn insert_part_with_batch_qty(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    status: &str,
    batch_status: &str,
    qty: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let batch_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-001', $4, $7, $3, $5, $5, 1, 0, $6, $6)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, $4, 0, $5, $5)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(qty)
    .bind(batch_status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    (part_id, batch_id)
}