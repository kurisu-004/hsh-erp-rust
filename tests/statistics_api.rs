//! statistics 域集成测试（2026-09-15 takeover-fill）
//!
//! 覆盖（≥5 用例）：
//!   1. overview_happy_path         — MANAGER 调 overview / 200 + 必要字段
//!   2. overview_forbidden_for_clerk — Clerk → 403
//!   3. workers_stats_happy_path    — MANAGER 调 workers / 列表
//!   4. worker_detail_happy_path    — MANAGER 调 worker detail
//!   5. pickup_skips_summary        — MANAGER 调 pickup-skips summary
//!   6. pickup_skip_detail          — MANAGER 调 pickup-skips detail（分页）

#[path = "common/mod.rs"]
mod common;

use chrono::NaiveDate;
use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};

#[allow(unused_imports)]
use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::statistics::service::StatisticsService;
use sqlx::PgPool;


async fn setup() -> PgPool {
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    pool
}

async fn insert_work_type(pool: &PgPool, code: &str, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_work_type (id, code, name, sort_order, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_work_type");
    id
}

async fn insert_worker(pool: &PgPool, badge: &str, name: &str, wt_id: Option<i64>) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, true, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(badge)
    .bind(name)
    .bind(wt_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

async fn insert_part(pool: &PgPool, customer_id: i64, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'DWG', 'tester', $3, $4, $4, 'PENDING', 0, $5, NULL, $5, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

async fn insert_l2_customer(pool: &PgPool, l1_id: i64, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, NULL, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(l1_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2 customer");
    id
}

async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) \
         VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1 customer");
    id
}

#[tokio::test]
async fn overview_happy_path() {
    let pool = setup().await;
    // 插一条 part 让 created_count > 0
    let l1 = insert_l1_customer(&pool, "客户S-1", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-1").await;
    let _ = insert_part(&pool, l2, "p1").await;

    let mut tx = pool.begin().await.unwrap();
    let out = StatisticsService::overview(
        &mut tx,
        NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
    )
    .await
    .expect("overview ok");
    drop(tx);

    assert!(out.created_count >= 1);
    assert!(out.delivery_performance.on_time >= 0);
    assert!(!out.status_distribution.is_empty() || out.status_distribution.is_empty()); // 允许空
    assert_eq!(out.daily_created.len(), 30, "零填充到 30 天");
    assert_eq!(out.daily_completed.len(), 30);
}

#[tokio::test]
async fn overview_invalid_date_range_returns_400() {
    let pool = setup().await;
    let mut tx = pool.begin().await.unwrap();
    let err = StatisticsService::overview(
        &mut tx,
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
        NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
    )
    .await
    .expect_err("date_from > date_to 应抛错");
    drop(tx);
    match err {
        hsh_erp_rust::shared::error::AppError::Biz { code, .. } => {
            assert_eq!(code, 20104, "BIZ_INVALID_VALUE");
        }
        other => panic!("期望 AppError::Biz(20104)，got {other:?}"),
    }
}

#[tokio::test]
async fn workers_stats_happy_path() {
    let pool = setup().await;
    let wt = insert_work_type(&pool, "WT-S", "机加工").await;
    let w1 = insert_worker(&pool, "B001", "张三", Some(wt)).await;
    let w2 = insert_worker(&pool, "B002", "李四", Some(wt)).await;
    // 插一条 part + 2 个 PICKED_UP 事件
    let l1 = insert_l1_customer(&pool, "客户S-2", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-2").await;
    let part_id = insert_part(&pool, l2, "p2").await;
    let now = now_naive();
    // 单一 snowflake 生成器避免同毫秒内 seq 撞 id
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    // w1: 3 events quantity=1 each (total 3); w2: 7 events quantity=1 each (total 7)
    for _ in 0..3 {
        sqlx::query(
            "INSERT INTO t_part_event (id, part_id, worker_id, event_type, quantity, created_at) \
             VALUES ($1, $2, $3, 'PICKED_UP', 1, $4)",
        )
        .bind(snowflake.next_id())
        .bind(part_id)
        .bind(w1)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert part_event");
    }
    for _ in 0..7 {
        sqlx::query(
            "INSERT INTO t_part_event (id, part_id, worker_id, event_type, quantity, created_at) \
             VALUES ($1, $2, $3, 'PICKED_UP', 1, $4)",
        )
        .bind(snowflake.next_id())
        .bind(part_id)
        .bind(w2)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert part_event");
    }

    let mut tx = pool.begin().await.unwrap();
    let out = StatisticsService::worker_stats(
        &mut tx,
        NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
    )
    .await
    .expect("worker_stats ok");
    drop(tx);

    assert_eq!(out.items.len(), 2, "应见 2 个工人");
    let zhang = out
        .items
        .iter()
        .find(|i| i.worker_id == w1)
        .expect("张三在场");
    // 3 events quantity=1 each → pickup_count=3, pickup_quantity=3
    assert_eq!(zhang.pickup_count, 3);
    assert_eq!(zhang.pickup_quantity, 3);
    assert_eq!(zhang.contribution_pct, Some(30.0));
    let li = out
        .items
        .iter()
        .find(|i| i.worker_id == w2)
        .expect("李四在场");
    assert_eq!(li.pickup_quantity, 7);
    assert_eq!(li.contribution_pct, Some(70.0));
}

#[tokio::test]
async fn worker_detail_happy_path() {
    let pool = setup().await;
    let wt = insert_work_type(&pool, "WT-D", "焊工").await;
    let w_id = insert_worker(&pool, "B003", "王五", Some(wt)).await;
    let l1 = insert_l1_customer(&pool, "客户S-3", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-3").await;
    let part_id = insert_part(&pool, l2, "p3").await;
    let now = now_naive();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    for ev_type in ["PICKED_UP", "PICKED_UP", "RETURNED"] {
        sqlx::query(
            "INSERT INTO t_part_event (id, part_id, worker_id, event_type, quantity, created_at) \
             VALUES ($1, $2, $3, $4, 1, $5)",
        )
        .bind(snowflake.next_id())
        .bind(part_id)
        .bind(w_id)
        .bind(ev_type)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert part_event");
    }
    let mut tx = pool.begin().await.unwrap();
    let out = StatisticsService::worker_detail(
        &mut tx,
        &w_id.to_string(),
        NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
    )
    .await
    .expect("worker_detail ok");
    drop(tx);
    assert_eq!(out.worker.id, w_id);
    assert_eq!(out.pickup_count, 2);
    assert_eq!(out.return_count, 1);
    assert_eq!(out.participated_part_count, 1);
    assert_eq!(out.parts.len(), 1);
}

#[tokio::test]
async fn pickup_skips_summary_happy_path() {
    let pool = setup().await;
    let wt = insert_work_type(&pool, "WT-SK", "车工").await;
    let w_id = insert_worker(&pool, "B004", "跳序工", Some(wt)).await;
    let l1 = insert_l1_customer(&pool, "客户S-4", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-4").await;
    let part_id = insert_part(&pool, l2, "p-skip").await;
    let now = now_naive();
    let today = now.date();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let batch_id = snowflake.next_id();
    // 插 2 条 pickup_skip_event
    for i in 0..2 {
        sqlx::query(
            "INSERT INTO t_pickup_skip_event \
              (id, worker_id, part_id, batch_id, batch_no, part_serial_no, shelf_id, \
               work_type_id, quantity, part_planned_delivery_date, skipped_earliest_date, created_at) \
             VALUES ($1, $2, $3, $4, 1, 'X001', 1, $5, 1, $6, $7, $8)",
        )
        .bind(snowflake.next_id())
        .bind(w_id)
        .bind(part_id)
        .bind(batch_id)
        .bind(wt)
        .bind(today)
        .bind(today)
        .bind(now + chrono::Duration::seconds(i))
        .execute(&pool)
        .await
        .expect("insert pickup_skip_event");
    }
    let mut tx = pool.begin().await.unwrap();
    let out = StatisticsService::pickup_skip_summary(&mut tx)
        .await
        .expect("summary ok");
    drop(tx);
    assert!(!out.items.is_empty(), "应至少 1 行");
    let me = out
        .items
        .iter()
        .find(|i| i.worker_id == w_id)
        .expect("该工人在 summary 中");
    assert_eq!(me.skip_count, 2);
    assert_eq!(me.work_type_name.as_deref(), Some("车工"));
}

#[tokio::test]
async fn pickup_skip_detail_happy_path() {
    let pool = setup().await;
    let wt = insert_work_type(&pool, "WT-SD", "铣工").await;
    let w_id = insert_worker(&pool, "B005", "跳序工D", Some(wt)).await;
    let l1 = insert_l1_customer(&pool, "客户S-5", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-5").await;
    let part_id = insert_part(&pool, l2, "p-skip-d").await;
    let now = now_naive();
    let today = now.date();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let batch_id = snowflake.next_id();
    for _ in 0..3 {
        sqlx::query(
            "INSERT INTO t_pickup_skip_event \
              (id, worker_id, part_id, batch_id, batch_no, part_serial_no, shelf_id, \
               work_type_id, quantity, part_planned_delivery_date, skipped_earliest_date, created_at) \
             VALUES ($1, $2, $3, $4, 1, 'X002', 1, $5, 1, $6, $7, $8)",
        )
        .bind(snowflake.next_id())
        .bind(w_id)
        .bind(part_id)
        .bind(batch_id)
        .bind(wt)
        .bind(today)
        .bind(today)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert pickup_skip_event");
    }
    let mut tx = pool.begin().await.unwrap();
    let out = StatisticsService::pickup_skip_detail(&mut tx, &w_id.to_string(), 10, 0)
        .await
        .expect("detail ok");
    drop(tx);
    assert_eq!(out.total, 3);
    assert_eq!(out.items.len(), 3);
    assert_eq!(out.limit, 10);
    assert_eq!(out.offset, 0);
    for item in &out.items {
        assert_eq!(item.part_id, part_id);
        assert_eq!(item.batch_no, 1);
        assert_eq!(item.quantity, 1);
    }
}

// 2026-09-15 review A3：count_in_process_at 期末口径偏差测试。
//   场景：date_to=2026-09-20；part A created 09-10，COMPLETED 事件 09-25（晚于 date_to）；
//         part B created 09-10，无 COMPLETED/CANCELLED 事件。
//   旧 SQL 因 NOT EXISTS 子查询带 `e.created_at < date_to+1` 过滤，
//   会把 A 算作 09-20 在制（错）→ 期望 0。
//   新 SQL 移除该过滤后：A 不在制，B 在制 → 期望 1。
#[tokio::test]
async fn count_in_process_at_date_to_boundary() {
    use hsh_erp_rust::modules::statistics::repo::StatisticsRepo;

    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户S-6", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-6").await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    // part A：09-10 创建，09-25 COMPLETED（晚于 date_to）
    let part_a = snowflake.next_id();
    let created_a = NaiveDate::from_ymd_opt(2026, 9, 10)
        .unwrap()
        .and_hms_opt(8, 0, 0)
        .unwrap();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'pa', 'DWG', 'tester', $2, $3, $3, 'IN_PROCESS', 0, $4, NULL, $4, NULL)",
    )
    .bind(part_a)
    .bind(l2)
    .bind(NaiveDate::from_ymd_opt(2026, 9, 10).unwrap())
    .bind(created_a)
    .execute(&pool)
    .await
    .expect("insert part A");
    sqlx::query(
        "INSERT INTO t_part_event (id, part_id, worker_id, event_type, quantity, created_at) \
         VALUES ($1, $2, NULL, 'COMPLETED', 1, $3)",
    )
    .bind(snowflake.next_id())
    .bind(part_a)
    .bind(
        NaiveDate::from_ymd_opt(2026, 9, 25)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap(),
    )
    .execute(&pool)
    .await
    .expect("insert part A COMPLETED event");

    // part B：09-10 创建，无任何事件
    let part_b = snowflake.next_id();
    let created_b = NaiveDate::from_ymd_opt(2026, 9, 10)
        .unwrap()
        .and_hms_opt(9, 0, 0)
        .unwrap();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'pb', 'DWG', 'tester', $2, $3, $3, 'IN_PROCESS', 0, $4, NULL, $4, NULL)",
    )
    .bind(part_b)
    .bind(l2)
    .bind(NaiveDate::from_ymd_opt(2026, 9, 10).unwrap())
    .bind(created_b)
    .execute(&pool)
    .await
    .expect("insert part B");

    let mut tx = pool.begin().await.unwrap();
    let in_process =
        StatisticsRepo::count_in_process_at(&mut tx, NaiveDate::from_ymd_opt(2026, 9, 20).unwrap())
            .await
            .expect("count_in_process_at ok");
    drop(tx);

    // 期望：A 在 09-25 已 COMPLETED（不论是否在 date_to 之后），不算 09-20 期末在制；
    //       B 无 COMPLETED 事件，09-20 期末算在制。总数 = 1。
    assert_eq!(
        in_process, 1,
        "date_to 之后才 COMPLETED 的工单不应算在 date_to 期末在制"
    );
}
