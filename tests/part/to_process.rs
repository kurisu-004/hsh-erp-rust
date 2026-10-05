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
use hsh_erp_test_support::{
    json_request, load_part_fixture, login_token, send, test_app, test_pool, test_state,
};

// ===========================================================================
//  动态 fixture helpers（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::{create_chain_for_part / create_step}`
//  引入 2 helper，因 fixtures.rs 本轮被删，复制到本地（同形 sqlx::query 直插）。
// ===========================================================================

/// 为指定 part 建一个最小工艺链（t_part_process_chain），并把 part.process_chain_id 绑回。
async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
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
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
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
    let (_part_id, batch_id) = insert_part_with_batch(
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
            &format!("/prod/batches/{batch_id}/to-process"),
            Some(json!({
                "shelf_id": "abc",
                "next_process_id": "1",
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
    let (_part_id, batch_id) = insert_part_with_batch(
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
            &format!("/prod/batches/{batch_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": "abc",
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
            &format!("/prod/batches/{batch_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": fx.process_id.to_string(),
                "note": "test fail",
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
/// to_process 是品检打回流，要求存在 INSPECTION 批次。
/// 锚定批次不是 INSPECTION 状态 → service 在 step 4 `find_inspection_batch_by_id`
/// 抛 20109 BIZ_PART_BATCH_NOT_FOUND（HTTP 404）。
#[tokio::test]
async fn to_process_wrong_state_rejected() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // setup: PENDING part（非 INSPECTION）
    let (_part_id, batch_id) = insert_part_with_batch(
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
            &format!("/prod/batches/{batch_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": fx.process_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body={body}");
    assert_eq!(body["code"], 20109, "BIZ_PART_BATCH_NOT_FOUND: {body}");
}

/// 2026-10-06 批次锚定回归：状态机闸门必须读 `batch.status`（真源）而非
/// `t_part.status`（min-progress 派生缓存列）。
///
/// 形态：`t_part.status` = `IN_PROCESS`（派生值）、被操作批次 = `INSPECTION`
/// → to-process（品检打回）必须**放行**（200）。改前 `IN_PROCESS → IN_PROCESS`
/// 无状态机边，恒被 20103 误拒。
///
/// 本文件的 `insert_part_with_batch` 本就分开接收 part status 与 batch status，
/// 故无需新 helper —— 只需传一组两列不同的值。
#[tokio::test]
async fn to_process_ignores_part_derived_status_when_batch_is_in_inspection() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // part 派生列 = IN_PROCESS；被操作批次 = INSPECTION
    let (part_id, batch_id) = insert_part_with_batch(
        &pool,
        "P0",
        fx.customer_l2_id,
        Some("P000"),
        "IN_PROCESS", // ← t_part.status：派生值
        "INSPECTION", // ← 批次真源：被操作对象
    )
    .await;
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": fx.process_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "批次本身是 INSPECTION 就该放行，不该被 part 派生列误拒: {body}"
    );
    assert_eq!(body["code"], 0, "to-process 应成功: {body}");

    let bs = sqlx::query_scalar::<_, String>("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("batch");
    assert_eq!(bs, "IN_PROCESS", "被操作批次应已翻到 IN_PROCESS: {body}");
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
    // to_process 有链时把 current_process_step_id 写成链内该工序的活跃 step（无链则落 NULL）
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": fx.process_id.to_string(),
                "quantity": 3,
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

/// to-process 拒绝**返修中**的批次 → 400 / 20118（2026-10-01 review 第 2 轮 MAJOR-3）。
///
/// 背景：REPAIRING 降级为 `is_repairing` 标记列后，返修批次的 `status` 就是
/// `IN_PROCESS` / `INSPECTION`，`to-process` 的 `allowed_from = ["INSPECTION"]`
/// 拦不住它了。可达链：`start-repair` → `to-inspection`（送检**保持**标记，
/// `mark_batch_inspected` 的 `is_repairing: None`）→ 本端点。放行的话批次会落到
/// 生产架却仍挂「返修中」标记（`GET /prod/batches/repairing` 长期显示异常、
/// `complete-repair` 仍接受它）。
///
/// 断言三件事：
/// 1. HTTP 400 + `code == 20118`（与「重复起修」同族的「返修流转前置条件不满足」）；
/// 2. 消息点名 `complete-repair`（前端据此引导用户改调哪个端点）；
/// 3. **零副作用**：批次仍是 `INSPECTION` + `is_repairing = true` + 未挂生产架。
#[tokio::test]
async fn to_process_rejects_repairing_batch() {
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
    // 造「送检中的返修件」形态：status='INSPECTION' + is_repairing=true
    // （与 `tests/part/repair.rs` 里直插标记列的做法同形）。
    sqlx::query("UPDATE t_part_batch SET is_repairing = true WHERE id = $1")
        .bind(batch_id)
        .execute(&pool)
        .await
        .expect("flag batch as repairing");
    // part 绑工艺链 + step：保证**唯一**的拒绝理由是返修守卫（若守卫被删，
    // 本请求会一路成功 → 测试红）。
    let chain_id = create_chain_for_part(&pool, part_id).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/to-process"),
            Some(json!({
                "shelf_id": fx.production_shelf_id.to_string(),
                "next_process_id": fx.process_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], 20118, "BIZ_PART_REPAIR_NOT_TRIGGERED: {body}");
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("complete-repair"),
        "消息必须点名改调 complete-repair（前端据此引导）: {msg}"
    );

    // 零副作用
    let (bstatus, is_repairing, location): (String, bool, Option<String>) =
        sqlx::query_as("SELECT status, is_repairing, location FROM t_part_batch WHERE id = $1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .expect("read batch");
    assert_eq!(bstatus, "INSPECTION", "被拒的请求不得改批次状态");
    assert!(is_repairing, "被拒的请求不得清/改返修标记");
    assert_eq!(location, None, "被拒的请求不得把批次挂到生产架");
}
