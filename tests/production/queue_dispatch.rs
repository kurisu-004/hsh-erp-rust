//! prod::batch 域端到端集成测试（2026-09-29 + 2026-09-30 重构）
//!
//! 覆盖场景：
//!   1. happy path：建 1 个 PENDING batch → `GET pending` 拿到 → `POST dispatch`（targets.length==1）成功
//!   2. dispatch 不存在 batch_id → 20121 BIZ_BATCH_NOT_FOUND (HTTP 404) → failed 数组
//!   3. dispatch 二次调用同 batch → 20120 BIZ_BATCH_INVALID_STATUS (HTTP 409) → failed 数组
//!   4. dispatch 时 target_process_id 在 t_shelf_process 0 结果 → 20508 (HTTP 404) → failed 数组
//!   5. dispatch 空 targets → 40001 VALIDATION_ERROR (HTTP 422)
//!   6. dispatch 一条 OK + 一条已软删 → succeeded=[], failed=[40404]；OK batch 保持 PENDING（事务回滚）
//!   7. auto-dispatch_preview 无 chain 的 batch → skip_reason=NO_PROCESS_CHAIN（只读，不写库）
//!   8. auto-dispatch_preview 空 batch_ids → 40001 VALIDATION_ERROR (HTTP 422)
//!   9. OCC 40901 VERSION_CONFLICT（双事务并发，A 持有 tx 占用 batch 行，B 调 dispatch 应 40901）
//!  10. 角色守卫：Inspector 调 dispatch → 40300 FORBIDDEN
//!  11. GET pending：unauth → 40100
//!  12. 回归（2026-09-30 review 第 3 轮 L5）：**无工序链工单** dispatch 后出现在
//!      `GET /prod/queue/{process_id}` + `/prod/queue/counts`（用户报告的原始 bug）
//!  13. 2026-10-04 `current_holder_id` 写脏守卫：dispatch 的目标货架已软删 / 已停用 /
//!      是品检区 → 20508 拒收且批次保持 PENDING + holder 仍 NULL
//!  14. 2026-10-04「跳过」语义：sort_order 最小的候选不可用时继续往后找可用货架，
//!      而不是把整个下发打成失败
//!
//! 2026-09-30 重构：
//! - dispatch 统一 bulk-only（targets 数组）；响应 `DispatchResult { succeeded, failed }`
//! - auto-dispatch 改为只读 `auto_dispatch_preview`；不再真实下发，仅返回 preview
//! - bulk-dispatch 端点已删除（路由层不再挂载）
//!
//! ## 串行化
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex / `--test-threads=1` 双保险。
//!
//! ## 集成测试范本（PR13 Phase F，2026-09-23）
//! 本文件按 Phase F 范本收敛：所有 HTTP / fixture helper 一律
//! `use hsh_erp_test_support::{...}`，**不保留本地副本**。
//! `prod::batch` 域独享的 raw SQL 构造（`insert_part` / `insert_part_batch` /
//! `insert_shelf_process_mapping` / `link_shelf_to_process`）因与 in-source
//! tests 同源且不在跨 binary 重用面，按惯例保留为本地 fn。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_test_support::{
    ProductionFixture, json_request, load_production_fixture, login_token, send, test_app,
    test_pool, test_state,
};

// ===========================================================================
//  Bootstrap helpers（PR13 Phase F 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 production fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 起一份 fresh database + 加载 production fixture + 以 INSPECTOR 身份登录。
///
/// 复用 production fixture 的 part 域基线（INSPECTOR 用户由 part fixture 提供）。
async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(
        &app,
        &fx.part_inspector_username,
        ProductionFixture::PASSWORD,
    )
    .await;
    (pool, app, token, fx)
}

// ===========================================================================
//  prod::batch 域独享 helper（绕开 part 域 CRUD）
// ===========================================================================

/// 直插一个最小 `t_customer` L2 行（绕开 com::customer CRUD）。
async fn insert_customer_l2(pool: &PgPool, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：ID 统一从全进程共享 generator 取（`shared_test_snowflake`），
    // 不再就地 new —— 两个 fresh generator 同 instance 同毫秒各取 seq 0 就撞
    // 对应表的 pkey（23505），与「同一个 helper 调几次」无关。
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, version, created_at, updated_at) \
         VALUES ($1, $2, 0, $3, $3)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_customer L2");
    id
}

/// 直插一个最小 `t_part` 行（status='PENDING'，带 planned_delivery_date；
/// 不挂 part_event / chain 等周边表）。
async fn insert_part(pool: &PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：ID 统一从全进程共享 generator 取（`shared_test_snowflake`），
    // 不再就地 new —— 两个 fresh generator 同 instance 同毫秒各取 seq 0 就撞
    // 对应表的 pkey（23505），与「同一个 helper 调几次」无关。
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, request_date, \
         planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, 'BATCH-TEST-PART', 'BATCH-DWG', '', 1, CURRENT_DATE, CURRENT_DATE, $2, \
         'PENDING', 0, $3, $3)",
    )
    .bind(id)
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part PENDING");
    id
}

/// 直插一个最小 `t_part_batch` 行（status='PENDING'，location=NULL）。
async fn insert_part_batch(pool: &PgPool, part_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：ID 统一从全进程共享 generator 取（`shared_test_snowflake`），
    // 不再就地 new —— 两个 fresh generator 同 instance 同毫秒各取 seq 0 就撞
    // 对应表的 pkey（23505），与「同一个 helper 调几次」无关。
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, 1, 'PENDING', 0, $3, $3)",
    )
    .bind(id)
    .bind(part_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch PENDING");
    id
}

/// 直插一个 `t_shelf` + `t_shelf_process` 映射（绕开 shelf CRUD）。
async fn insert_shelf_process_mapping(pool: &PgPool, process_id: i64) -> i64 {
    insert_shelf_process_mapping_state(
        pool,
        "BATCH-TEST-SHELF",
        process_id,
        "PRODUCTION",
        true,
        false,
        0,
    )
    .await
}

/// 2026-10-04 新增：可指定货架状态的「工序 → 货架」映射构造。
///
/// 供 `current_holder_id` 写脏守卫的回归用例用：dispatch 解析货架走
/// `ShelfProcessRepo::find_first_shelf_for_process`，该方法自 2026-10-04 起
/// `JOIN t_shelf` 并带 `deleted_at IS NULL` + `is_active` + `zone='PRODUCTION'`
/// 三个谓词，故测试必须能造出「映射行 active 但货架不可用」的三种形态。
///
/// 沿既有 helper 的做法：直插静态行（`sqlx::query` 运行时宏，不动 `.sqlx`）。
/// `code` 参与形参是因为 `t_shelf.code` 有唯一约束（20502 BIZ_SHELF_DUPLICATE_CODE）
/// —— 同一个用例内要造多个货架时必须各自不同。
#[allow(clippy::too_many_arguments)]
async fn insert_shelf_process_mapping_state(
    pool: &PgPool,
    code: &str,
    process_id: i64,
    zone: &str,
    is_active: bool,
    deleted: bool,
    sort_order: i32,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：同上，两个 ID 从同一个共享 generator 连续取号（顺序不变）。
    let shelf_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let mapping_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at, deleted_at) VALUES ($1, $2, $2, $3, $4, 0, 0, $5, $5, \
         CASE WHEN $6 THEN $5::timestamp ELSE NULL END)",
    )
    .bind(shelf_id)
    .bind(code)
    .bind(zone)
    .bind(is_active)
    .bind(now)
    .bind(deleted)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
         created_at, updated_at) VALUES ($1, $2, $3, $4, 0, $5, $5)",
    )
    .bind(mapping_id)
    .bind(shelf_id)
    .bind(process_id)
    .bind(sort_order)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
    shelf_id
}

/// 直插一条**链 + 链首 step**，返回 `(chain_id, step_id)`。
///
/// 2026-10-09：dispatch 的不变式改成「有链时按链首工序下发」，故集成测试必须能
/// 造出「已绑链的 PENDING 批次」。`sort_order` 用 10（稀疏）—— dispatch 取链首
/// 按 `sort_order ASC, id ASC LIMIT 1`，与端点 5 `preview_auto_dispatch` 同形。
async fn insert_chain_with_step(pool: &PgPool, process_id: i64) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let chain_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let step_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, 'chain-disp', 0, $2, 0, $2, 0)",
    )
    .bind(chain_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_process_chain");
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 10, $3, 30, 0, $4, 0, $4, 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process_chain_step");
    (chain_id, step_id)
}

/// 把 part 绑到某条链上（`insert_part` 建的是无链 part）。
async fn bind_part_to_chain(pool: &PgPool, part_id: i64, chain_id: i64) {
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("bind part to chain");
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: happy path —— GET pending → POST dispatch → IN_PROCESS + 事件写入
#[tokio::test]
async fn dispatch_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    let customer_id = insert_customer_l2(&pool, "ACME").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    let _shelf_id = insert_shelf_process_mapping(&pool, process_a).await;

    // 1. GET pending
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/queue/pending?limit=200&offset=0",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "list pending: {env1}");
    assert_eq!(env1["data"]["total"], 1);
    assert_eq!(env1["data"]["items"][0]["batch_id"], batch_id.to_string());

    // 2. POST dispatch（2026-09-30 bulk-only 形态：targets 数组）
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }],
                "note": "happy path test",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "dispatch: {env2}");
    assert_eq!(env2["data"]["succeeded"].as_array().unwrap().len(), 1);
    // failed 字段在空数组时 skip_serializing_if 省略（partial commit 预留字段）
    let failed_len = env2["data"]["failed"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(failed_len, 0);
    let succeeded = &env2["data"]["succeeded"][0];
    assert_eq!(succeeded["batch_id"], batch_id.to_string());
    assert_eq!(succeeded["target_process_id"], process_a.to_string());
    // version 应从 0 → 1
    assert_eq!(succeeded["version"], 1);

    // 3. DB 验证：batch 应 IN_PROCESS
    let row: (String, Option<i64>) =
        sqlx::query_as("SELECT status, current_holder_id FROM t_part_batch WHERE id = $1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, "IN_PROCESS");

    // 4. 事件验证：PLACED_ON_SHELF 应写入
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_event WHERE batch_id = $1 AND event_type = 'PLACED_ON_SHELF'",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "应写 1 条 PLACED_ON_SHELF 事件");
}

/// 场景 2: dispatch 不存在 batch_id → 20121 (HTTP 404)
#[tokio::test]
async fn dispatch_nonexistent_batch_returns_not_found() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": "9999999999999",
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    // 2026-09-30 重构：service 任一失败抛 AppError → handler 不 commit → 响应顶层 code
    assert_eq!(s, StatusCode::NOT_FOUND, "不存在 batch 应 404: {env}");
    assert_eq!(env["code"], 20121, "BIZ_BATCH_NOT_FOUND: {env}");
}

/// 场景 3: dispatch 二次调用同 batch → 20120 BIZ_BATCH_INVALID_STATUS (HTTP 409)
#[tokio::test]
async fn dispatch_second_call_returns_invalid_status() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    let customer_id = insert_customer_l2(&pool, "ACME").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    let _shelf_id = insert_shelf_process_mapping(&pool, process_a).await;

    // 1. 第 1 次 dispatch：OK
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "第 1 次 dispatch: {env1}");
    assert_eq!(env1["data"]["succeeded"].as_array().unwrap().len(), 1);

    // 2. 第 2 次 dispatch：batch.status='IN_PROCESS' → 20120 → failed
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    // 2026-09-30 重构：service 抛 AppError → 顶层响应 code = 20120
    assert_eq!(s2, StatusCode::CONFLICT, "第 2 次 dispatch 应 409: {env2}");
    assert_eq!(env2["code"], 20120, "BIZ_BATCH_INVALID_STATUS: {env2}");
}

/// 场景 4: dispatch 时 target_process_id 在 t_shelf_process 0 结果 → 20508 (HTTP 404)
#[tokio::test]
async fn dispatch_no_shelf_for_process_returns_not_found() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;
    // 不插入 shelf_process 映射 → 0 结果

    let customer_id = insert_customer_l2(&pool, "ACME").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    // 2026-09-30 重构：service 抛 AppError → 顶层响应 code = 20508
    assert_eq!(s, StatusCode::NOT_FOUND, "无货架映射应 404: {env}");
    assert_eq!(env["code"], 20508, "BIZ_SHELF_PROCESS_NOT_FOUND: {env}");

    // 验证 batch 仍是 PENDING（事务回滚）
    let row: (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "PENDING", "dispatch 失败后 batch 应保持 PENDING");
}

/// 场景 5 (2026-09-30 重构): dispatch 空 targets → 40001 VALIDATION_ERROR (HTTP 422)
///
/// 2026-09-30 之前：bulk-dispatch 端点专用测试；重构后 dispatch 统一 bulk-only 形态，
/// 「bulk-only」=「targets 数组」，空 targets 仍触发 validation。
#[tokio::test]
async fn dispatch_empty_targets_returns_validation_error() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({ "targets": [] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "空 targets 应 422: {env}"
    );
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
}

/// 场景 6 (2026-09-30 重构): dispatch bulk 形态：一条 OK + 一条已软删 → succeeded=[], failed=[40404]
///
/// 2026-09-30 之前：bulk-dispatch 端点专用测试。重构后走统一 dispatch 端点（bulk-only）。
#[tokio::test]
async fn dispatch_bulk_full_rollback_on_one_failure() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    let customer_id = insert_customer_l2(&pool, "ACME").await;
    let part_ok = insert_part(&pool, customer_id).await;
    let batch_ok = insert_part_batch(&pool, part_ok).await;
    let part_bad = insert_part(&pool, customer_id).await;
    let batch_bad = insert_part_batch(&pool, part_bad).await;
    let _shelf_id = insert_shelf_process_mapping(&pool, process_a).await;

    // 软删 batch_bad
    sqlx::query("UPDATE t_part_batch SET deleted_at = now() WHERE id = $1")
        .bind(batch_bad)
        .execute(&pool)
        .await
        .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [
                    { "batch_id": batch_ok.to_string(), "target_process_id": process_a.to_string() },
                    { "batch_id": batch_bad.to_string(), "target_process_id": process_a.to_string() },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    // 2026-09-30 重构：service 任一失败抛 AppError，handler tx Drop 全回滚 → 顶层 code
    assert_eq!(s, StatusCode::NOT_FOUND, "batch_bad 已软删应 404: {env}");
    assert_eq!(env["code"], 20121, "BIZ_BATCH_NOT_FOUND: {env}");

    // DB 验证：batch_ok 应保持 PENDING（事务回滚）
    let row_ok: (String,) = sqlx::query_as("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(batch_ok)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_ok.0, "PENDING", "batch_ok 应保持 PENDING（事务回滚）");
}

/// 场景 7 (2026-09-30 重构): auto-dispatch_preview 无 chain → skip_reason=NO_PROCESS_CHAIN
///
/// 2026-09-30 之前：auto-dispatch 真下发并返回 succeeded/skipped 数组。
/// 重构后改为只读查询，返回 items[*].skip_reason，前端据此构造 dispatch 请求。
#[tokio::test]
async fn auto_dispatch_preview_no_chain_returns_skip_reason() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let _ = fx;

    let customer_id = insert_customer_l2(&pool, "ACME").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    // part.process_chain_id NULL（insert_part 默认）

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-dispatch",
            Some(json!({
                "batch_ids": [batch_id.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto-dispatch preview OK: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["skip_reason"], "NO_PROCESS_CHAIN");
    // 2026-09-30 review 第 1 轮：NO_PROCESS_CHAIN 时 process_chain_id 必为 JSON null
    // （不再输出字符串 "0"）。
    assert!(
        items[0]["process_chain_id"].is_null(),
        "NO_PROCESS_CHAIN 时 process_chain_id 应为 null，实际: {}",
        items[0]["process_chain_id"]
    );
    assert!(items[0]["first_process_id"].is_null());
    assert!(items[0]["first_shelf_id"].is_null());

    // DB 验证：batch 应保持 PENDING（preview 不写库）
    let row: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row, "PENDING", "preview 不应改变 batch.status");
}

/// 场景 7b (2026-09-30 新增): auto-dispatch_preview 完整链路 → first_process_id/first_shelf_id 透传
#[tokio::test]
async fn auto_dispatch_preview_with_chain_returns_first_process_and_shelf() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    // 手动建一个货架 + 映射到 process_a（fixture 不预置 shelf_a_id）
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：原为 `SnowflakeIdGenerator::new(1_577_836_800_000, 7)`（靠换 instance
    // 避撞的权宜写法），现统一走全进程共享 generator —— 下面 3 个 ID 从同一对象
    // 连续取号，相对顺序不变。
    let now = now_naive();
    let shelf_a = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) VALUES ($1, 'PREVIEW-SH', 'PREVIEW-SH', 'PRODUCTION', true, 0, 0, $2, $2)",
    )
    .bind(shelf_a)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
         created_at, updated_at) VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(hsh_erp_test_support::shared_test_snowflake().next_id())
    .bind(shelf_a)
    .bind(process_a)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    // 插入一个 PENDING batch
    let customer_id = insert_customer_l2(&pool, "ACME-PREVIEW").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;

    // 构造 part.process_chain_id + chain step + 工艺链首道指向 process_a
    let chain_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 0, $2, 1, $2, 1)",
    )
    .bind(chain_id)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, estimated_minutes, version, \
         created_at, created_by, updated_at, updated_by) VALUES ($1, $2, 1, $3, 0, 0, $4, 1, $4, 1)",
    )
    .bind(hsh_erp_test_support::shared_test_snowflake().next_id())
    .bind(chain_id)
    .bind(process_a)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-dispatch",
            Some(json!({
                "batch_ids": [batch_id.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "preview OK: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert!(item["skip_reason"].is_null(), "完整链路不应 skip: {env}");
    // 2026-09-30 review 第 1 轮：OK 路径三个 ID 必为字符串（雪花 ID 序列化）。
    assert_eq!(item["process_chain_id"], chain_id.to_string());
    assert_eq!(item["first_process_id"], process_a.to_string());
    assert_eq!(item["first_shelf_id"], shelf_a.to_string());

    // DB 验证：batch 仍 PENDING
    let row: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row, "PENDING");
}

/// 场景 7c (2026-09-30 新增): auto-dispatch_preview 不存在的 batch_id → skip_reason=NOT_FOUND
#[tokio::test]
async fn auto_dispatch_preview_unknown_batch_returns_not_found() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-dispatch",
            Some(json!({ "batch_ids": ["9999999999999"] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "preview OK: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["skip_reason"], "NOT_FOUND");
    // 2026-09-30 review 第 1 轮：NOT_FOUND 时三个 ID 必为 null。
    assert!(items[0]["process_chain_id"].is_null());
    assert!(items[0]["first_process_id"].is_null());
    assert!(items[0]["first_shelf_id"].is_null());
}

/// 场景 8 (2026-09-30 重构): auto-dispatch_preview 空 batch_ids → 40001 VALIDATION_ERROR (HTTP 422)
#[tokio::test]
async fn auto_dispatch_preview_empty_batch_ids_returns_validation_error() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/auto-dispatch",
            Some(json!({ "batch_ids": [] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "空 batch_ids 应 422: {env}"
    );
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
}

/// 场景 9: 并发前置 mutate（status 已被另一事务改成 IN_PROCESS）→ 20120
///
/// 模拟「另一事务已完成 mutation → dispatch_batch 的 status 守卫直接拒绝」这一
/// 并发前置场景。PG 默认 READ COMMITTED 隔离级别下，dispatch_batch 的 fetch 会
/// 读到最新 status=IN_PROCESS，service 第 3 步 status 守卫直接返回
/// BIZ_BATCH_INVALID_STATUS。
///
/// 注：纯 OCC 40901（version 在 fetch 后、UPDATE 前被另一事务 mutate）无法在
/// 单连接单线程单测里稳定构造——其触发条件需要「dispatch_batch fetch → 另一事务
/// commit → dispatch_batch UPDATE」三步在毫秒级内真实交错。集成测试侧用本测试
/// 覆盖 status 守卫路径 + in-source tests 覆盖 fetch 0 行的特殊情况，40901 纯
/// OCC 路径由 production E2E 并发压测兜底。详见 service.rs 同名测试的注释。
#[tokio::test]
async fn dispatch_after_concurrent_status_change_returns_invalid_status() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    let customer_id = insert_customer_l2(&pool, "ACME").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    let _shelf_id = insert_shelf_process_mapping(&pool, process_a).await;

    // 模拟另一事务已完成 mutation：status='IN_PROCESS'
    sqlx::query("UPDATE t_part_batch SET status = 'IN_PROCESS' WHERE id = $1")
        .bind(batch_id)
        .execute(&pool)
        .await
        .unwrap();

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "并发前置 mutate 后应 409: {env}");
    assert_eq!(env["code"], 20120, "BIZ_BATCH_INVALID_STATUS: {env}");
}

/// 场景 9b: 外部 version bump → dispatch_batch 仍成功（version 自适应）
///
/// 模拟「另一事务仅 bump version 但未改 status」的场景：dispatch_batch 入口
/// fetch 读到最新 version（如 7），UPDATE WHERE version=7 命中 1 行，
/// 返回 `version = 7 + 1 = 8`。这是 OCC 在 READ COMMITTED 隔离级别下的
/// 「乐观」语义：每次 fetch 都拿到最新 version，重试路径在 caller 侧
/// （前端拿到新 version 后再次 dispatch）。
#[tokio::test]
async fn dispatch_after_external_version_bump_succeeds_with_new_version() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    let customer_id = insert_customer_l2(&pool, "ACME").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    let _shelf_id = insert_shelf_process_mapping(&pool, process_a).await;

    // 外部 mutate：version 0 → 7（status 保持 PENDING）
    sqlx::query("UPDATE t_part_batch SET version = 7 WHERE id = $1")
        .bind(batch_id)
        .execute(&pool)
        .await
        .unwrap();

    // dispatch_batch fetch 读到 version=7，UPDATE WHERE version=7 命中 1 行，OK
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "version 7 时 dispatch 应 OK: {env}");
    assert_eq!(
        env["data"]["succeeded"][0]["version"], 8,
        "version 应 7 → 8"
    );
}

/// 场景 10: 角色守卫 —— Inspector 调 dispatch → 40300 FORBIDDEN
#[tokio::test]
async fn dispatch_forbidden_for_inspector() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let _ = fx;
    let process_a = fx.process_a_id;

    let customer_id = insert_customer_l2(&pool, "ACME").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    let _shelf_id = insert_shelf_process_mapping(&pool, process_a).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "Inspector 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// 场景 11: GET pending 未登录 → 40100 UNAUTHORIZED
#[tokio::test]
async fn list_pending_unauth_returns_401() {
    let (_pool, app, _token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(app, json_request("GET", "/prod/queue/pending", None, None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "未登录应 401: {env}");
    assert_eq!(env["code"], 40100, "UNAUTHORIZED: {env}");
}

/// 场景 12（2026-09-30 review 第 3 轮 L5）**旗舰场景端到端回归**：
/// **无工序链工单**的批次 dispatch 到某工序后，必须出现在该工序的候选池里
/// （`GET /prod/queue/{process_id}` 的 items + `GET /prod/queue/counts` 的 count）。
///
/// 这就是用户报告的原始 bug（「拖批次下发到工序后，工序池不显示该批次」），
/// 也是本次改动的核心目标：**让没有工序链的工单，其批次也能正常入池**。
///
/// 旧设计下之所以入不了池：dispatch 写 `current_process_step_id = NULL`（无 chain
/// 解析不出 step），而 3 条候选池 SQL 全部 `INNER JOIN t_process_chain_step
/// ON s.id = pb.current_process_step_id` → `s.id = NULL` 匹配不到任何行 → 批次对
/// 所有池查询隐身。
///
/// 修复后 dispatch 写 `current_process_id = target_process_id`，池 SQL 改为按该列
/// 普通过滤。**回退 `update_batch_dispatched` 的 `current_process_id = $5` 即会让本
/// 测试必红** —— 此前 `prod/batch/service.rs` 只在单测里断言该列、prod::queue 的
/// helper 又都建了 chain + step，没有任何端到端测试覆盖这个组合。
#[tokio::test]
async fn dispatch_part_without_process_chain_appears_in_pool() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    let customer_id = insert_customer_l2(&pool, "ACME-NOCHAIN").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    let _shelf_id = insert_shelf_process_mapping(&pool, process_a).await;

    // 前置断言：本场景的关键前提是「工单没有工序链」
    let chain_id: Option<i64> =
        sqlx::query_scalar("SELECT process_chain_id FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("read t_part.process_chain_id");
    assert_eq!(
        chain_id, None,
        "前置条件：part 应**无工序链**（process_chain_id IS NULL），\
         否则本测试退化成有链场景、验不到本分支要支持的那条路径"
    );

    // 1. dispatch 到 process_a
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "dispatch: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["succeeded"].as_array().unwrap().len(),
        1,
        "dispatch 应成功: {env}"
    );
    // 响应体也回显池归属权威列
    assert_eq!(
        env["data"]["succeeded"][0]["current_process_id"],
        process_a.to_string(),
        "DispatchSuccessItem.current_process_id 应 = target_process_id: {env}"
    );
    // step 仍为 null（无链解析不出）—— 刻意如此，且不影响入池
    assert_eq!(
        env["data"]["succeeded"][0]["current_process_step_id"],
        serde_json::Value::Null,
        "无工序链时 step 应为 null（可选显示用定位信息，不影响入池）: {env}"
    );

    // 2. DB 层：IN_PROCESS + PRODUCTION_SHELF + holder=shelf + cpid=目标工序
    let (status, location, holder, cpid, step): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, location, current_holder_id, current_process_id, \
         current_process_step_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("read dispatched batch");
    assert_eq!(status, "IN_PROCESS", "dispatched batch 应 IN_PROCESS");
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(holder, Some(_shelf_id), "holder 应为解析出的货架");
    assert_eq!(
        cpid,
        Some(process_a),
        "current_process_id 应 = target_process_id（入池权威依据）"
    );
    assert_eq!(step, None, "无工序链 → step 恒 NULL");

    // 3. 端点层：批次出现在目标工序池（这是用户报告症状的正脸）。
    //    2026-10-08：`GET /pool/{process_id}` 已删，改打聚合端点
    //    `GET /prod/queue/processes/{process_id}`。
    let (ps, pool_env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/prod/queue/processes/{process_a}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        ps,
        StatusCode::OK,
        "GET queue/processes/{process_a}: {pool_env}"
    );
    let items = pool_env["data"]["items"].as_array().expect("data.items");
    let batch_id_str = batch_id.to_string();
    assert!(
        items.iter().any(|i| i["batch_id"] == batch_id_str),
        "无工序链工单的批次下发后应出现在工序 {process_a} 候选池: {pool_env}"
    );

    // 4. 序列板同样应计入（前端 tab 徽标）。`GET /pool/counts` 已删，
    //    `GET /prod/queue/snapshot` 的 `processes[].pool_count` 取代。
    let (cs, snap_env) = send(
        app,
        json_request("GET", "/prod/queue/snapshot", None, Some(&token)),
    )
    .await;
    assert_eq!(cs, StatusCode::OK, "GET queue/snapshot: {snap_env}");
    let processes = snap_env["data"]["processes"]
        .as_array()
        .expect("data.processes");
    let process_a_str = process_a.to_string();
    let hit = processes
        .iter()
        .find(|c| c["process_id"] == process_a_str)
        .unwrap_or_else(|| {
            panic!(
                "processes 应含 process_id={process_a}（否则 tab 徽标为 0，\
                 与用户报告的症状一致）: {snap_env}"
            )
        });
    let count: i64 = hit["pool_count"]
        .as_i64()
        .expect("pool_count 应为 JSON integer");
    assert!(
        count >= 1,
        "process {process_a} 的候选批次数应 ≥ 1，实际 {count}: {snap_env}"
    );
}

/// 2026-10-09：dispatch 的新不变式 —— **已绑链工单按链首工序下发**，step 指针落
/// 链首 step，请求里的 `target_process_id` 被忽略（它只是无链时的回落值）。
///
/// 与场景 12（无链回落）成对：两条路径一起钉住，「有链走链首 / 无链走请求」这个
/// 二分不会被单边改动悄悄改掉。请求传的是 `process_b`（**不是**链首），若实现
/// 回落了它，本测试的 `current_process_id` / 货架两项断言都会红。
#[tokio::test]
async fn dispatch_chained_part_uses_chain_head_process_and_step() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let head_process = fx.process_a_id; // 链首工序
    let requested_process = fx.process_b_id; // 请求里传的那道（应被忽略）

    let customer_id = insert_customer_l2(&pool, "ACME-CHAIN").await;
    let part_id = insert_part(&pool, customer_id).await;
    let (chain_id, head_step_id) = insert_chain_with_step(&pool, head_process).await;
    bind_part_to_chain(&pool, part_id, chain_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    // 两道工序都配货架映射：差异只在「用谁的工序去解析货架」
    let head_shelf = insert_shelf_process_mapping_state(
        &pool,
        "SH-CHAIN-HEAD",
        head_process,
        "PRODUCTION",
        true,
        false,
        0,
    )
    .await;
    insert_shelf_process_mapping_state(
        &pool,
        "SH-CHAIN-REQ",
        requested_process,
        "PRODUCTION",
        true,
        false,
        0,
    )
    .await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": requested_process.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "dispatch: {env}");
    let item = &env["data"]["succeeded"][0];

    // 出参：step 指针 = 链首 step id（本字段是 JSON number，不带字符串化器）
    assert_eq!(
        item["current_process_step_id"],
        json!(head_step_id),
        "有链工单 dispatch 后 current_process_step_id 应 = 链首 step id: {env}"
    );
    assert_eq!(
        item["current_process_id"],
        head_process.to_string(),
        "有链工单按链首工序下发（≠ 请求里的 target_process_id）: {env}"
    );
    assert_eq!(
        item["shelf_id"],
        head_shelf.to_string(),
        "货架按链首工序解析: {env}"
    );

    // DB 层：权威锚点在库里（出参可能只是回显）
    let (cpid, step): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("read dispatched batch");
    assert_eq!(cpid, Some(head_process));
    assert_eq!(step, Some(head_step_id));
}

/// 2026-10-09：**无链工单** dispatch 仍按请求的 `target_process_id` 回落，
/// `current_process_step_id` 落 NULL —— 与上条成对，逐字保持旧行为。
///
/// 场景 12 已经从 DB 层钉了这两列；本条补的是**出参**层（`DispatchSuccessItem`
/// 的两个字段），因为出参这轮从「诚实置 None」改成了「填真实写入值」，改错了
/// 只有断言出参才测得出来。
///
/// ⚠️ **fixture 刻意给批次预置一个陈旧 step 指针**（2026-10-09 review 第 1 轮补）：
/// 只断言「dispatch 后 step 是 NULL」钉不住 `clear_process_step_id` 的取值 ——
/// 指针本来就是 NULL 时，`clear = true`（写 NULL）与 `clear = false`（`COALESCE`
/// 保留原值）结果完全一样。预置一个**真实存在过的 step** 才能区分：只有
/// `clear = true` 才会把它洗成 NULL。
///
/// 这个形态不是臆造的：2026-10-09 之前 dispatch 一律清 NULL，但同期 worker-scan
/// RETURNED / 外协发送等写点会写 step，工单解绑链之后就会留下这种「无链 + 指针非
/// NULL」的陈旧数据。也正因为这条不变式（「无链 ⇒ step 恒 NULL」）没有任何约束保证，
/// 无链侧才必须显式清 NULL 而不能保留原值（见 `QueueDispatchRepo::
/// update_batch_dispatched` 的 doc）。
#[tokio::test]
async fn dispatch_no_chain_part_falls_back_to_target_process_and_null_step() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    let customer_id = insert_customer_l2(&pool, "ACME-NOCHAIN2").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;
    insert_shelf_process_mapping(&pool, process_a).await;

    // 造陈旧指针：建链 + step → 把批次指针指过去 → 把 part 从链上解绑。
    // 终态是「无链工单 + step 指针非 NULL」，正是 clear=true 要清洗的形态。
    let (chain_id, stale_step_id) = insert_chain_with_step(&pool, process_a).await;
    bind_part_to_chain(&pool, part_id, chain_id).await;
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = $1 WHERE id = $2")
        .bind(stale_step_id)
        .bind(batch_id)
        .execute(&pool)
        .await
        .expect("seed stale step pointer");
    sqlx::query("UPDATE t_part SET process_chain_id = NULL WHERE id = $1")
        .bind(part_id)
        .execute(&pool)
        .await
        .expect("unbind part from chain");
    // 前置：确认造出来的确实是「无链 + 指针非 NULL」，否则后面的断言会假绿
    let pre: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT p.process_chain_id, pb.current_process_step_id \
         FROM t_part_batch pb JOIN t_part p ON p.id = pb.part_id WHERE pb.id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("read pre-dispatch state");
    assert_eq!(pre.0, None, "前置：工单应无链");
    assert_eq!(pre.1, Some(stale_step_id), "前置：批次应带陈旧 step 指针");

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": process_a.to_string(),
                }]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "dispatch: {env}");
    let item = &env["data"]["succeeded"][0];
    assert_eq!(
        item["current_process_id"],
        process_a.to_string(),
        "无链工单按请求的 target_process_id 下发: {env}"
    );
    assert_eq!(
        item["current_process_step_id"],
        serde_json::Value::Null,
        "无链工单没有链首可落，step 出参应为 null: {env}"
    );
    let step: Option<i64> =
        sqlx::query_scalar("SELECT current_process_step_id FROM t_part_batch WHERE id = $1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .expect("read dispatched batch");
    assert_eq!(
        step, None,
        "无链工单 → 陈旧 step 指针必须被洗成 NULL（clear_process_step_id = true）"
    );
}

// ===========================================================================
//  2026-10-04 `current_holder_id` 写脏守卫：dispatch 解析货架
// ===========================================================================
//
// 背景：`ShelfProcessRepo::find_first_shelf_for_process` 自 2026-10-04 起
// `JOIN t_shelf` 并带 `deleted_at IS NULL` + `is_active=true` + `zone='PRODUCTION'`。
// 收紧前它只看 `t_shelf_process` 行是否软删，于是「映射行 active、货架已停用 /
// 已软删 / 是品检架」这三种形态都会被下发并写进 `t_part_batch.current_holder_id`。
// 后果不是报错而是**静默漏件**：报工台取件页数据源
// （`part::service::phase1::work_type` 的 pickable-by-work-type）硬限定
// `JOIN t_shelf sh ON sh.id = b.current_holder_id AND sh.is_active = true
//   AND sh.zone = 'PRODUCTION'`，故这类批次永远不出现在工人的可领列表里。
//
// 断言三件事：① 20508 拒收；② `t_part_batch` 未被写脏（仍 PENDING + holder NULL）；
// ③ 候选里混着不可用货架时是「跳过」而不是「整笔失败」。

/// 三种「映射 active 但货架不可用」形态：已软删 / 已停用 / 品检区。
///
/// 逐个独立跑（同一个用例内跑三遍，走三个 fresh batch）——三者是**不同谓词**
/// （`deleted_at` / `is_active` / `zone`）各自的回归，合并成一个断言会让失败时看不出
/// 是哪条谓词漏了。
#[tokio::test]
async fn dispatch_rejects_unusable_shelf_in_all_three_shapes() {
    // (用例名, zone, is_active, deleted, 期望文案关键词)
    let cases: [(&str, &str, bool, bool, &str); 3] = [
        ("soft-deleted", "PRODUCTION", true, true, "已软删"),
        ("inactive", "PRODUCTION", false, false, "已停用"),
        (
            "inspection-zone",
            "INSPECTION",
            true,
            false,
            "非 PRODUCTION 区",
        ),
    ];

    for (name, zone, is_active, deleted, kw) in cases {
        let (pool, app, token, fx) = bootstrap_as_manager().await;
        let process_a = fx.process_a_id;
        // 造 1 条映射，货架按用例指定的状态
        let bad_shelf = insert_shelf_process_mapping_state(
            &pool,
            &format!("SH-BAD-{name}"),
            process_a,
            zone,
            is_active,
            deleted,
            0,
        )
        .await;

        let customer_id = insert_customer_l2(&pool, "BATCH-GUARD").await;
        let part_id = insert_part(&pool, customer_id).await;
        let batch_id = insert_part_batch(&pool, part_id).await;

        let (s, env) = send(
            app.clone(),
            json_request(
                "POST",
                "/prod/queue/dispatch",
                Some(json!({ "targets": [
                    { "batch_id": batch_id.to_string(), "target_process_id": process_a.to_string() }
                ] })),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{name}: 不可用货架应 404: {env}");
        assert_eq!(
            env["code"].as_i64().unwrap(),
            20508,
            "{name}: 应复用 20508 BIZ_SHELF_PROCESS_NOT_FOUND（不新造码）: {env}"
        );
        let msg = env["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains(kw),
            "{name}: 错误文案要指出「{kw}」，否则运营会去查错方向: {env}"
        );

        // 批次未被写脏：仍 PENDING + holder 仍 NULL + version 未动
        let (status, location, holder, version): (
            String,
            Option<String>,
            Option<i64>,
            i32,
        ) = sqlx::query_as(
            "SELECT status, location, current_holder_id, version FROM t_part_batch WHERE id = $1",
        )
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("read batch after rejected dispatch");
        assert_eq!(status, "PENDING", "{name}: 拒收后 batch 应仍 PENDING");
        assert_eq!(location, None, "{name}: 拒收后 location 应仍 NULL");
        assert_eq!(
            holder, None,
            "{name}: current_holder_id 必须仍 NULL（未被写脏）"
        );
        assert_eq!(version, 0, "{name}: 拒收后 version 不应被自增");
        let _ = bad_shelf;
    }
}

/// 「跳过」语义：sort_order 最小的候选货架不可用时，应继续往后找到可用的那个，
/// 而不是把整个下发打成失败。
///
/// 这条锁的是 `find_first_shelf_for_process` 把守卫写在 **SQL 的 WHERE** 上（而不是
/// 在 service 层取首条再报错）的取舍 —— 后者在「第一个候选恰好被停用」这种极常见的
/// 运维场景下会把本可自动恢复的下发升级成阻塞。
#[tokio::test]
async fn dispatch_skips_unusable_shelf_and_uses_next_candidate() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let process_a = fx.process_a_id;

    // sort_order=0 的品检架（守卫要跳过）+ sort_order=1 的正常生产架
    let _bad = insert_shelf_process_mapping_state(
        &pool,
        "SH-SKIP-BAD",
        process_a,
        "INSPECTION",
        true,
        false,
        0,
    )
    .await;
    let good = insert_shelf_process_mapping_state(
        &pool,
        "SH-SKIP-GOOD",
        process_a,
        "PRODUCTION",
        true,
        false,
        1,
    )
    .await;

    let customer_id = insert_customer_l2(&pool, "BATCH-SKIP").await;
    let part_id = insert_part(&pool, customer_id).await;
    let batch_id = insert_part_batch(&pool, part_id).await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({ "targets": [
                { "batch_id": batch_id.to_string(), "target_process_id": process_a.to_string() }
            ] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "有可用候选时应下发成功: {env}");
    assert_eq!(
        env["data"]["succeeded"][0]["shelf_id"].as_str().unwrap(),
        good.to_string(),
        "应跳过品检架、选中下一个 PRODUCTION 候选: {env}"
    );
    let holder: Option<i64> =
        sqlx::query_scalar("SELECT current_holder_id FROM t_part_batch WHERE id = $1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .expect("read holder");
    assert_eq!(holder, Some(good), "holder 必须是那个可用的生产架");
}
