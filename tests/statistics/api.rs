//! statistics 域集成测试（2026-09-15 takeover-fill）
//!
//! 覆盖（4 用例）：
//!   1. overview_invalid_date_range_returns_400 — date_from > date_to → BIZ 20104
//!   2. pickup_skips_summary_happy_path         — MANAGER 调 pickup-skips summary
//!   3. pickup_skip_detail_happy_path           — pickup-skips detail（分页）
//!   4. count_in_process_at_date_to_boundary    — 期末在制口径（date_to 边界）
//!
//! ⚠️ **覆盖空洞（2026-10-10）**：`StatisticsService::overview` / `worker_stats` /
//! `worker_detail` **无 happy-path 集成测试**。原三条把查询窗口硬编码成绝对日期、
//! 而插入的行用 `now_naive()`，跨月后窗口里查不到行（2026-09 绿、2026-10-01 起红），
//! 已删除而非修复——用户决定接受该覆盖清零。补回来时**不得**硬编码绝对日期窗口：
//! 要么照 `tests/statistics/event_driven.rs` 的 `insert_delivered_event(…, at_date)`
//! 钉住插入时间戳，要么让窗口由 `now_naive()` 推导。
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除 `clean_db` / `clean_business_db` /
//! `ensure_database_exists` 三件套调用（`test_pool()` 走 fresh_database_url，
//! 进程级 plan 2 隔离天然给出空库，无需手动 clean），统一走
//! `use hsh_erp_test_support::{...}` + `load_statistics_fixture(&pool)`。
//! 保留本地 helper `insert_work_type` / `insert_worker` / `insert_part` /
//! `insert_l1_customer` / `insert_l2_customer`：statistics 域测试每个用例
//! 都要按需造不同 code / badge / prefix / name 的行（与 fixture 预置的
//! FX-WT-STAT baseline 不同），保留本地 fn 直插。
//!
//! ## 不预置 part / batch / event / pickup_skip_event
//! statistics 域状态机不允许从 IN_PROCESS / COMPLETED 回退 PENDING，且事件行
//! (event_type / worker_id / created_at) 由测试现场按场景构造，fixture 故不
//! 预置这些行，保留为本地 fn 直插以避免污染「期望空库」断言。

use chrono::NaiveDate;

use hsh_erp_test_support::{load_statistics_fixture, shared_test_snowflake, test_pool};

#[allow(unused_imports)]
use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::modules::statistics::service::StatisticsService;
use sqlx::PgPool;

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 statistics fixture，返回 `(pool, fx)`。
///
/// service 层直调用例不需要 `app` / `token`（不走 HTTP），故 bootstrap 只返
/// `(pool, fx)`；`fx` 暴露 baseline ID（WORK_TYPE_ID / WORKER_ID 等）供
/// 偶发复用场景使用，绝大多数用例仍走本地 helper 自建不同 code / prefix。
async fn setup() -> (PgPool, hsh_erp_test_support::fixture::StatisticsFixture) {
    let pool = test_pool().await;
    let fx = load_statistics_fixture(&pool).await;
    (pool, fx)
}

// ===========================================================================
//  statistics 域独享 helper（按场景造不同 code / badge / prefix / name）
//
//  2026-10-09：本节所有 ID 一律从 `shared_test_snowflake()`（全进程唯一 generator 对象）
//  取号，不再就地 `SnowflakeIdGenerator::new(...)`。原先 5 个 helper 各自 fresh、instance
//  又都是 1，同毫秒各取 seq 0 就发出逐字节相同的 id —— `insert_l1_customer` /
//  `insert_l2_customer` 同写 `t_customer`，是最典型的碰撞对（各调一次即撞，不是「同一个
//  helper 调两次」才撞）。
// ===========================================================================

async fn insert_work_type(pool: &PgPool, code: &str, name: &str) -> i64 {
    let id = shared_test_snowflake().next_id();
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
    let id = shared_test_snowflake().next_id();
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
    let id = shared_test_snowflake().next_id();
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
    let id = shared_test_snowflake().next_id();
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
    let id = shared_test_snowflake().next_id();
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
async fn overview_invalid_date_range_returns_400() {
    let (pool, _fx) = setup().await;
    let mut tx = pool.begin().await.unwrap();
    let err = StatisticsService
        .overview(
            &mut *tx,
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
async fn pickup_skips_summary_happy_path() {
    let (pool, _fx) = setup().await;
    let wt = insert_work_type(&pool, "WT-SK", "车工").await;
    let w_id = insert_worker(&pool, "B004", "跳序工", Some(wt)).await;
    let l1 = insert_l1_customer(&pool, "客户S-4", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-4").await;
    let part_id = insert_part(&pool, l2, "p-skip").await;
    let now = now_naive();
    let today = now.date();
    let batch_id = shared_test_snowflake().next_id();
    // 插 2 条 pickup_skip_event
    for i in 0..2 {
        sqlx::query(
            "INSERT INTO t_pickup_skip_event \
              (id, worker_id, part_id, batch_id, batch_no, part_serial_no, shelf_id, \
               work_type_id, quantity, part_planned_delivery_date, skipped_earliest_date, created_at) \
             VALUES ($1, $2, $3, $4, 1, 'X001', 1, $5, 1, $6, $7, $8)",
        )
        .bind(shared_test_snowflake().next_id())
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
    let out = StatisticsService
        .pickup_skip_summary(&mut *tx)
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
    let (pool, _fx) = setup().await;
    let wt = insert_work_type(&pool, "WT-SD", "铣工").await;
    let w_id = insert_worker(&pool, "B005", "跳序工D", Some(wt)).await;
    let l1 = insert_l1_customer(&pool, "客户S-5", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-5").await;
    let part_id = insert_part(&pool, l2, "p-skip-d").await;
    let now = now_naive();
    let today = now.date();
    let batch_id = shared_test_snowflake().next_id();
    for _ in 0..3 {
        sqlx::query(
            "INSERT INTO t_pickup_skip_event \
              (id, worker_id, part_id, batch_id, batch_no, part_serial_no, shelf_id, \
               work_type_id, quantity, part_planned_delivery_date, skipped_earliest_date, created_at) \
             VALUES ($1, $2, $3, $4, 1, 'X002', 1, $5, 1, $6, $7, $8)",
        )
        .bind(shared_test_snowflake().next_id())
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
    let out = StatisticsService
        .pickup_skip_detail(&mut *tx, &w_id.to_string(), 10, 0)
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
    use hsh_erp_rust::modules::statistics::repo::sql as statistics_sql;

    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户S-6", "F").await;
    let l2 = insert_l2_customer(&pool, l1, "子客S-6").await;

    // part A：09-10 创建，09-25 COMPLETED（晚于 date_to）
    let part_a = shared_test_snowflake().next_id();
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
    .bind(shared_test_snowflake().next_id())
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
    let part_b = shared_test_snowflake().next_id();
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
        statistics_sql::count_in_process_at(&mut tx, NaiveDate::from_ymd_opt(2026, 9, 20).unwrap())
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
