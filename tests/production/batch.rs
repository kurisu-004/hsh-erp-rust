//! prod::batch 域端到端集成测试（2026-09-29）
//!
//! 覆盖场景：
//!   1. happy path：建 1 个 PENDING batch → `GET pending` 拿到 → `POST dispatch` 成功
//!   2. dispatch 不存在 batch_id → 20121 BIZ_BATCH_NOT_FOUND (HTTP 404)
//!   3. dispatch 二次调用同 batch → 20120 BIZ_BATCH_INVALID_STATUS (HTTP 409)
//!   4. dispatch 时 target_process_id 在 t_shelf_process 0 结果 → 20508 (HTTP 404)
//!   5. bulk-dispatch 空 targets → 40001 VALIDATION_ERROR (HTTP 422)
//!   6. bulk-dispatch 全回滚：一条 OK + 一条已软删 → 全失败，b_ok 保持 PENDING
//!   7. auto-dispatch 无 chain 的 batch → skipped(reason=NO_PROCESS_CHAIN)
//!   8. auto-dispatch 空 batch_ids → 40001 VALIDATION_ERROR (HTTP 422)
//!   9. OCC 40901 VERSION_CONFLICT（双事务并发，A 持有 tx 占用 batch 行，B 调 dispatch 应 40901）
//!  10. 角色守卫：Inspector 调 dispatch → 40300 FORBIDDEN
//!  11. GET pending：unauth → 40100
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
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
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
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
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
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
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
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let shelf_id = snowflake.next_id();
    let mapping_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) VALUES ($1, 'BATCH-TEST-SHELF', 'BATCH-TEST-SHELF', \
         'PRODUCTION', true, 0, 0, $2, $2)",
    )
    .bind(shelf_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
         created_at, updated_at) VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(mapping_id)
    .bind(shelf_id)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
    shelf_id
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
            "/prod/batches/pending?limit=200&offset=0",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "list pending: {env1}");
    assert_eq!(env1["data"]["total"], 1);
    assert_eq!(env1["data"]["items"][0]["batch_id"], batch_id.to_string());

    // 2. POST dispatch
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/dispatch",
            Some(json!({
                "batch_id": batch_id.to_string(),
                "target_process_id": process_a.to_string(),
                "note": "happy path test",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "dispatch: {env2}");
    let data = &env2["data"];
    assert_eq!(data["batch_id"], batch_id.to_string());
    assert_eq!(data["target_process_id"], process_a.to_string());
    // version 应从 0 → 1
    assert_eq!(data["version"], 1);

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
            "/prod/batches/dispatch",
            Some(json!({
                "batch_id": "9999999999999",
                "target_process_id": process_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
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
            "/prod/batches/dispatch",
            Some(json!({
                "batch_id": batch_id.to_string(),
                "target_process_id": process_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "第 1 次 dispatch: {env1}");

    // 2. 第 2 次 dispatch：batch.status='IN_PROCESS' → 20120
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/dispatch",
            Some(json!({
                "batch_id": batch_id.to_string(),
                "target_process_id": process_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
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
            "/prod/batches/dispatch",
            Some(json!({
                "batch_id": batch_id.to_string(),
                "target_process_id": process_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
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

/// 场景 5: bulk-dispatch 空 targets → 40001 VALIDATION_ERROR (HTTP 422)
#[tokio::test]
async fn bulk_dispatch_empty_targets_returns_validation_error() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/bulk-dispatch",
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

/// 场景 6: bulk-dispatch 全回滚：一条 OK + 一条已软删 → 全失败
#[tokio::test]
async fn bulk_dispatch_full_rollback_on_one_failure() {
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
            "/prod/batches/bulk-dispatch",
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
    // 任一失败 → 全回滚（404 BIZ_BATCH_NOT_FOUND）
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

/// 场景 7: auto-dispatch 无 chain 的 batch → skipped(reason=NO_PROCESS_CHAIN)
#[tokio::test]
async fn auto_dispatch_no_process_chain_returns_skipped() {
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
            "/prod/batches/auto-dispatch",
            Some(json!({
                "batch_ids": [batch_id.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto-dispatch OK: {env}");
    assert_eq!(env["data"]["succeeded"].as_array().unwrap().len(), 0);
    let skipped = env["data"]["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["reason"], "NO_PROCESS_CHAIN");
}

/// 场景 8: auto-dispatch 空 batch_ids → 40001 VALIDATION_ERROR (HTTP 422)
#[tokio::test]
async fn auto_dispatch_empty_batch_ids_returns_validation_error() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/batches/auto-dispatch",
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
            "/prod/batches/dispatch",
            Some(json!({
                "batch_id": batch_id.to_string(),
                "target_process_id": process_a.to_string(),
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
            "/prod/batches/dispatch",
            Some(json!({
                "batch_id": batch_id.to_string(),
                "target_process_id": process_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "version 7 时 dispatch 应 OK: {env}");
    assert_eq!(env["data"]["version"], 8, "version 应 7 → 8");
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
            "/prod/batches/dispatch",
            Some(json!({
                "batch_id": batch_id.to_string(),
                "target_process_id": process_a.to_string(),
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

    let (s, env) = send(
        app,
        json_request("GET", "/prod/batches/pending", None, None),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "未登录应 401: {env}");
    assert_eq!(env["code"], 40100, "UNAUTHORIZED: {env}");
}
