//! 2026-09-16 PR-2（part-slim-down）回归测试 —— statistics 域交付 / 逾期口径
//! 改按 t_part_event 事件派生（不再读 t_part.actual_delivery_date，已删列）。
//!
//! PR-2 § statistics/repo.rs：
//! - `delivered_stats`：CTE+LATERAL 子查询，按 t_part_event.event_type='DELIVERED'
//!   派生 actual_date。分类与不变：orange = 晚于 planned 且不晚于 system；
//!   red = 晚于 system。
//! - `count_overdue_undelivered`：NOT EXISTS DELIVERED 事件口径。
//!
//! 本测试覆盖：
//! 1. delivered_stats 按事件计数（含 on_time / orange / red 分类）
//! 2. count_overdue_undelivered 按事件口径判未交付

#[path = "common/mod.rs"]
mod common;

use chrono::NaiveDate;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::statistics::repo::sql as statistics_sql;

async fn setup() -> PgPool {
    common::ensure_database_exists().await;
    let pool = common::test_pool().await;
    common::clean_db(&pool).await;
    common::clean_business_db(&pool).await;
    pool
}

async fn insert_l2_customer(pool: &PgPool) -> (i64, i64) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let l1 = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(l1)
    .bind("STAT-L1")
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
    .bind("STAT-L2")
    .bind(l1)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2");
    (l1, l2)
}

/// 插 part（含 planned_delivery_date 与 system_delivery_date）。
///
/// 2026-09-16 PR-2：t_part 已删 `actual_delivery_date`，不再写入；交付日期由
/// t_part_event 派生。
async fn insert_part_with_dates(
    pool: &PgPool,
    customer_id: i64,
    name: &str,
    planned_date: NaiveDate,
    system_date: Option<NaiveDate>,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, system_delivery_date, status, quantity, \
         unit_price, total_price, version, created_at, updated_at) \
         VALUES ($1, $2, 'D-STAT', 'tester', $3, $4, $4, $5, 'PENDING', 1, 0, 0, 0, $6, $6)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(planned_date)
    .bind(system_date)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 在 part 上插一个 PENDING 初始批次（PR-B1 引入：每 part 必有 1 个批次）。
async fn insert_initial_batch(pool: &PgPool, part_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
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
    .expect("insert t_part_batch");
    id
}

/// 插一条 DELIVERED 事件（PR-2 § statistics/repo.rs::delivered_stats 的真相源）。
async fn insert_delivered_event(pool: &PgPool, part_id: i64, batch_id: i64, at_date: NaiveDate) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let event_at = at_date.and_hms_opt(12, 0, 0).unwrap();
    sqlx::query(
        "INSERT INTO t_part_event (id, part_id, worker_id, event_type, batch_id, quantity, \
         created_at) \
         VALUES ($1, $2, NULL, 'DELIVERED', $3, 1, $4)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_id)
    .bind(event_at)
    .execute(pool)
    .await
    .expect("insert DELIVERED event");
}

// ===========================================================================
//  Tests
// ===========================================================================

/// delivered_stats 按事件派生：3 part × 不同「事件日 vs 计划日」组合。
///
/// 分类口径（PR-2 § statistics/repo.rs::delivered_stats）：
/// - orange = `actual_date > planned AND (system IS NULL OR actual_date <= system)`
/// - red    = `system NOT NULL AND actual_date > system`
/// - on_time = `max(cnt - orange - red, 0)`（互斥拆分；on_time 不会作为 CASE 单独 SUM）
///
/// 场景：
///   P1: planned=2026-09-10, system=2026-09-15, DELIVERED @ 2026-09-12
///       → 晚于 planned 但不晚于 system → orange
///   P2: planned=2026-09-10, system=2026-09-15, DELIVERED @ 2026-09-16
///       → 晚于 system → red
///   P3: planned=2026-09-10, system=NULL,       DELIVERED @ 2026-09-11
///       → 晚于 planned + system IS NULL → orange（无 system 容差，逾期判 orange）
///
/// 期望：cnt=3, orange=2 (P1 + P3), red=1 (P2)。
/// 范围取 [2026-09-10, 2026-09-20] 全覆盖 3 个事件日。
#[tokio::test]
async fn delivered_stats_counts_via_delivered_events() {
    let pool = setup().await;
    let (_l1, l2) = insert_l2_customer(&pool).await;

    let p1 = insert_part_with_dates(
        &pool,
        l2,
        "P1",
        NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        Some(NaiveDate::from_ymd_opt(2026, 9, 15).unwrap()),
    )
    .await;
    let b1 = insert_initial_batch(&pool, p1).await;
    insert_delivered_event(&pool, p1, b1, NaiveDate::from_ymd_opt(2026, 9, 12).unwrap()).await;

    let p2 = insert_part_with_dates(
        &pool,
        l2,
        "P2",
        NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        Some(NaiveDate::from_ymd_opt(2026, 9, 15).unwrap()),
    )
    .await;
    let b2 = insert_initial_batch(&pool, p2).await;
    insert_delivered_event(&pool, p2, b2, NaiveDate::from_ymd_opt(2026, 9, 16).unwrap()).await;

    let p3 = insert_part_with_dates(
        &pool,
        l2,
        "P3",
        NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        None,
    )
    .await;
    let b3 = insert_initial_batch(&pool, p3).await;
    insert_delivered_event(&pool, p3, b3, NaiveDate::from_ymd_opt(2026, 9, 11).unwrap()).await;

    let mut tx = pool.begin().await.unwrap();
    let (cnt, _sum_total, orange, red) = statistics_sql::delivered_stats(
        &mut tx,
        NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        NaiveDate::from_ymd_opt(2026, 9, 20).unwrap(),
    )
    .await
    .expect("delivered_stats ok");
    drop(tx);

    assert_eq!(cnt, 3, "3 个 part 都有 DELIVERED 事件：{cnt}");
    assert_eq!(
        red, 1,
        "P2 实际晚于 system (09-16 > 09-15) → 1 个 red: {red}"
    );
    assert_eq!(
        orange, 2,
        "P1 (晚于 planned 但 ≤ system) + P3 (晚于 planned + system IS NULL) → orange=2: {orange}"
    );
}

/// count_overdue_undelivered 按 NOT EXISTS DELIVERED 事件口径：
///   P1: planned<today, 无 DELIVERED 事件 → 计入 overdue
///   P2: planned<today, 有 DELIVERED 事件 → 不计 overdue
///   P3: planned>=today, 无 DELIVERED 事件 → 不计 overdue（按状态过滤）
///
/// 期望：cnt=1（P1）。
#[tokio::test]
async fn count_overdue_undelivered_uses_event_absence() {
    let pool = setup().await;
    let (_l1, l2) = insert_l2_customer(&pool).await;
    let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();

    // P1: planned 早于 today, 无 DELIVERED 事件 → 计入
    let p1 = insert_part_with_dates(
        &pool,
        l2,
        "P1",
        NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        None,
    )
    .await;
    insert_initial_batch(&pool, p1).await;

    // P2: planned 早于 today, 但有 DELIVERED 事件 → 不计
    let p2 = insert_part_with_dates(
        &pool,
        l2,
        "P2",
        NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        None,
    )
    .await;
    let b2 = insert_initial_batch(&pool, p2).await;
    insert_delivered_event(&pool, p2, b2, NaiveDate::from_ymd_opt(2026, 9, 18).unwrap()).await;

    // P3: planned 晚于 today, 无 DELIVERED 事件 → 不计（按 planned<today 过滤）
    let p3 = insert_part_with_dates(
        &pool,
        l2,
        "P3",
        NaiveDate::from_ymd_opt(2026, 9, 25).unwrap(),
        None,
    )
    .await;
    insert_initial_batch(&pool, p3).await;

    let mut tx = pool.begin().await.unwrap();
    let cnt = statistics_sql::count_overdue_undelivered(&mut tx, today)
        .await
        .expect("count_overdue_undelivered ok");
    drop(tx);

    assert_eq!(
        cnt, 1,
        "仅 P1 满足「planned<today + 无 DELIVERED 事件」: {cnt}"
    );
}

/// 反例：part 软删后，count_overdue_undelivered 不计软删件（PR-2 § repo.rs:224
/// `p.deleted_at IS NULL`）。
#[tokio::test]
async fn count_overdue_undelivered_excludes_soft_deleted() {
    let pool = setup().await;
    let (_l1, l2) = insert_l2_customer(&pool).await;
    let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();

    // 1 个 part：planned 早于 today，无 DELIVERED 事件，但被软删
    let p1 = insert_part_with_dates(
        &pool,
        l2,
        "P1-DEL",
        NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
        None,
    )
    .await;
    insert_initial_batch(&pool, p1).await;
    sqlx::query("UPDATE t_part SET deleted_at = now(), version = version + 1 WHERE id = $1")
        .bind(p1)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let cnt = statistics_sql::count_overdue_undelivered(&mut tx, today)
        .await
        .expect("count_overdue_undelivered ok");
    drop(tx);

    assert_eq!(cnt, 0, "软删件不应计入 overdue 未交付: {cnt}");
}
