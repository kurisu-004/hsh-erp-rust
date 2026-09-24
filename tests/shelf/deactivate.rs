//! 2026-09-16 PR-2（part-slim-down）回归测试 —— worker / shelf 停用守卫
//! 改查 t_part_batch 真相源（PR-2 § shelf/repo.rs::count_in_use_parts、
//! worker/repo.rs::count_in_use_parts）。
//!
//! PR-2 之前：「被 X 引用」查 `t_part.current_holder_id`（已删列）。
//! PR-2 之后：改查 `t_part_batch.current_holder_id + location + status`：
//!   - shelf：location IN ('PRODUCTION_SHELF','INSPECTION_SHELF') + status IN
//!     ('IN_PROCESS','INSPECTION','REPAIRING')
//!   - worker：location='WORKER' + status IN
//!     ('IN_PROCESS','INSPECTION','REPAIRING','RETURNED')
//!
//! 任一 >0 ⇒ 20503 BIZ_SHELF_IN_USE / 20203 BIZ_WORKER_IN_USE。
//!
//! 本测试覆盖：
//! 1. shelf 被活跃 IN_PROCESS 批次持有 → deactivate 拒（20503）
//! 2. worker 被活跃 IN_PROCESS 批次持有 → deactivate 拒（20203）
//! 3. 反例：worker / shelf 无持有 → deactivate 通过（happy path 回归）
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 2026-09-24 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` /
//! `setup` / 通用 `login_manager` helper，统一走
//! `use hsh_erp_test_support::{...}` + `bootstrap_as_manager()` +
//! `load_shelf_fixture(&pool)`。保留：
//! - `insert_shelf`：shelf 域独享（绕开业务 API 直插 t_shelf；不复用
//!   `fixtures::insert_shelf` 是因为 Phase H gate 5 禁止 shelf 域从
//!   `fixtures` 模块 `use` 任何动态 helper —— fixture 范本要求只走 fixture
//!   + test-support http + bootstrap；本地 helper 用 sqlx::query 直插同形 SQL）
//! - `insert_l2_customer` / `insert_part` / `insert_batch` / `insert_worker_min`：
//!   worker/shelf 域独享（绕开业务 API，按需造不同 prefix / 不同 status /
//!   不同 badge；跨 binary 不重用，保留为本地 fn）

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    ShelfFixture, json_request, load_shelf_fixture, login_token, send, test_app, test_pool,
    test_state,
};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 shelf fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ShelfFixture) {
    let pool = test_pool().await;
    let fx = load_shelf_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ShelfFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  shelf fixture helpers（deactivate 域独享，跨 binary 不迁移）
// ===========================================================================

/// 直插一个 t_shelf 行（zone + code + name）。
///
/// 2026-09-24 PR13 Phase H：从 fixtures::insert_shelf 复制一份本地版本 ——
// Phase H gate 5 禁止 shelf 域从 `fixtures` 模块 `use` 任何动态 helper
/// （`insert_shelf` / `insert_part` / `add_role` 等）。fixture 范本要求测试
/// 只走 fixture + test-support http + bootstrap；本地 helper 用 `sqlx::query`
/// 直插与 fixtures::insert_shelf 同形 SQL（columns / defaults 全部对齐
/// migration 003 的 t_shelf schema）。
async fn insert_shelf(pool: &PgPool, code: &str, name: &str, zone: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(zone)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

/// 直插 L1（parent_id=NULL, prefix='F'）+ L2（parent_id=L1.id, prefix=NULL），
/// 绕开业务 API + 避开 `uq_t_customer_root_prefix`（part fixture L1 已占 prefix='P'）。
async fn insert_l2_customer(pool: &PgPool) -> (i64, i64) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let l1 = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(l1)
    .bind("WSDA-L1")
    .bind("F")
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1");
    let l2 = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, $3, NULL, 0, $4, $4)",
    )
    .bind(l2)
    .bind("WSDA-L2")
    .bind(l1)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2");
    (l1, l2)
}

/// 直插一个 t_part 行（IN_PROCESS 状态，customer_id 由 caller 指定）。
async fn insert_part(pool: &PgPool, customer_id: i64, name: &str, serial_no: Option<&str>) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-WSDA', 'tester', $4, $5, $5, 'IN_PROCESS', 1, 0, $6, $6)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 插 t_part_batch（status / location / holder 由 caller 指定）。
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    status: &str,
    location: &str,
    holder_id: Option<i64>,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 1, $3, $4, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(location)
    .bind(holder_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 直插一个最小 t_worker 行（无 work_type_id，由 worker_pool 域按需绑）。
async fn insert_worker_min(pool: &PgPool, badge: &str, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, true, NULL, 0, $4, $4)",
    )
    .bind(id)
    .bind(badge)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. shelf 被 IN_PROCESS 批次持有 → deactivate 拒（20503 BIZ_SHELF_IN_USE）。
///
/// PR-2 § shelf/repo.rs::count_in_use_parts：
/// `SELECT COUNT(*) FROM t_part_batch WHERE current_holder_id = $1
///    AND location IN ('PRODUCTION_SHELF','INSPECTION_SHELF')
///    AND status = 'IN_PROCESS' AND deleted_at IS NULL`。
#[tokio::test]
async fn shelf_deactivate_rejects_when_held_by_active_batch() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_l1, l2) = insert_l2_customer(&pool).await;
    let shelf_id = insert_shelf(&pool, "WSDA-SHELF", "WSDA", "PRODUCTION").await;

    let pid = insert_part(&pool, l2, "P0", Some("P0-SN")).await;
    // 让该 part 的活跃批次持有该 shelf（IN_PROCESS + PRODUCTION_SHELF）
    insert_batch(&pool, pid, "IN_PROCESS", "PRODUCTION_SHELF", Some(shelf_id)).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/shelves/{shelf_id}/deactivate"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "shelf 被 IN_PROCESS 批次持有 → deactivate 应拒: {env}"
    );
    assert_eq!(
        env["code"], 20503,
        "BIZ_SHELF_IN_USE（PR-2 count_in_use_parts 改 t_part_batch 后应仍命中）: {env}"
    );
}

/// 2. worker 被 IN_PROCESS 批次持有 → deactivate 拒（20203 BIZ_WORKER_IN_USE）。
///
/// PR-2 § worker/repo.rs::count_in_use_parts：
/// `SELECT COUNT(*) FROM t_part_batch WHERE current_holder_id = $1
///    AND location = 'WORKER'
///    AND status = 'IN_PROCESS' AND deleted_at IS NULL`。
#[tokio::test]
async fn worker_deactivate_rejects_when_holding_active_batch() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_l1, l2) = insert_l2_customer(&pool).await;
    let worker_id = insert_worker_min(&pool, "WSDA-BC001", "工-WSDA").await;

    let pid = insert_part(&pool, l2, "P0", Some("P0-SN")).await;
    insert_batch(&pool, pid, "IN_PROCESS", "WORKER", Some(worker_id)).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/workers/{worker_id}/deactivate"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "worker 持有 IN_PROCESS 批次 → deactivate 应拒: {env}"
    );
    assert_eq!(
        env["code"], 20203,
        "BIZ_WORKER_IN_USE（PR-2 count_in_use_parts 改 t_part_batch 后应仍命中）: {env}"
    );
}

/// 3. 反例：worker / shelf 无持有 → deactivate 通过（happy path 回归）。
///
/// 避免 PR-2 改查真相源后误把所有 deactivate 都拒。
#[tokio::test]
async fn shelf_deactivate_succeeds_when_no_active_holders() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let shelf_id = insert_shelf(&pool, "WSDA-EMPTY", "WSDA-empty", "PRODUCTION").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/shelves/{shelf_id}/deactivate"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "无持有 → deactivate 应通过: {env}");
    assert_eq!(env["code"], 0);
}

#[tokio::test]
async fn worker_deactivate_succeeds_when_holding_nothing() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let worker_id = insert_worker_min(&pool, "WSDA-BC002", "工-WSDA-empty").await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/workers/{worker_id}/deactivate"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "无持有 → deactivate 应通过: {env}");
    assert_eq!(env["code"], 0);
}