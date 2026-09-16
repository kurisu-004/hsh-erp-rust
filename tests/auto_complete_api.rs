//! `task::auto_complete::run_once` 集成测试
//!
//! 覆盖：
//! 1. `run_once_completes_overdue_delivered_batch` —— DELIVERED + placed_at 超阈值
//!    的批次被自动 COMPLETED；
//! 2. `run_once_skips_recent_batch` —— placed_at 未超阈值的不动；
//! 3. `run_once_emits_ws_event_after_commit` —— commit 后 ws_hub 收到 PART_COMPLETED。
//!
//! 集成测试运行需要 `tests/common::test_pool` + `ensure_database_exists`。

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use chrono::Duration as ChronoDuration;
use tokio::sync::broadcast::error::TryRecvError;

use common::{ensure_database_exists, test_pool, test_state_with_disabled_session};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsEvent;
use hsh_erp_rust::task::auto_complete::run_once;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 清空 auto_complete 涉及表（customer / part / batch）。
/// 用 CASCADE 兜底防止依赖漏列；测试库每次跑都从零开始。
async fn reset_auto_complete_state(pool: &sqlx::PgPool) {
    sqlx::query(
        "TRUNCATE t_customer, t_part_event, t_part_batch, t_part \
         RESTART IDENTITY CASCADE",
    )
    .execute(pool)
    .await
    .expect("truncate customer+part+batch");
}

/// 构造 L1 + L2 客户。L1 用唯一前缀（按时间戳后缀）防 `uq_t_customer_root_prefix` 冲突。
async fn seed_customer(pool: &sqlx::PgPool) -> (i64, i64) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    // 用 snowflake 后 5 位作 prefix 后缀，确保唯一（A-Z 仅 26 槽位，并行跑会撞）
    let suffix_id = snowflake.next_id();
    let suffix = ((suffix_id % 26) as u8 + b'A') as char;
    let l1_prefix = suffix.to_string();
    let l1 = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(l1)
    .bind(format!("AC-L1-{suffix_id}"))
    .bind(&l1_prefix)
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
    .bind(format!("AC-L2-{suffix_id}"))
    .bind(l1)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2");
    (l1, l2)
}

/// 插一个 DELIVERED 状态的 part + batch 行。
///
/// `placed_at` = `now_naive() - placed_days_ago * 86400s`（便于调阈值）。
/// 返回 `(part_id, batch_id)`。
async fn seed_delivered_batch(
    pool: &sqlx::PgPool,
    customer_id: i64,
    serial: &str,
    placed_days_ago: i64,
) -> (i64, i64) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let today = now.date();
    let part_id = snowflake.next_id();
    // 2026-09-16 PR-2（migration 027）：t_part 删 `actual_delivery_date` /
    // `has_been_repaired`；实际交付日期真相源改为 t_part_event.DELIVERED 事件。
    // 2026-09-16 PR-3（migration 028）：t_part_batch 删 `placed_at`；auto_complete
    // 阈值同步改走 DELIVERED 事件 created_at（与 Python `_run_once` 口径对齐）。
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, \
         quantity, version, created_at, updated_at) \
         VALUES ($1, $2, 'AC-TEST', 'D-001', $3, 'DELIVERED', 'TEST', $4, $4, \
                 1, 0, $5, $5)",
    )
    .bind(part_id)
    .bind(serial)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");

    // 2026-09-16 PR-3 批次 step 化：t_part_batch 删 `placed_at` 列；
    // auto_complete 阈值改读 t_part_event DELIVERED 事件 created_at（与 Python 口径对齐）。
    // 本 helper 改为同步插入一条 DELIVERED 事件，created_at = placed_at 旧值。
    let delivered_event_at = now - ChronoDuration::days(placed_days_ago);
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, \
         version, created_at, updated_at) \
         VALUES ($1, $2, 1, 1, 'DELIVERED', 0, $3, $3)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    // 同步插入 DELIVERED 事件（PR-3 新口径：auto_complete 按事件 created_at 判定）
    let event_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_event (id, part_id, batch_id, event_type, \
         from_status, to_status, quantity, created_at, created_by) \
         VALUES ($1, $2, $3, 'DELIVERED', 'READY_TO_SHIP', 'DELIVERED', 1, $4, NULL)",
    )
    .bind(event_id)
    .bind(part_id)
    .bind(batch_id)
    .bind(delivered_event_at)
    .execute(pool)
    .await
    .expect("insert DELIVERED event");

    (part_id, batch_id)
}

/// DELIVERED + placed_at 早于阈值（7 天）→ run_once 翻为 COMPLETED。
#[tokio::test]
async fn run_once_completes_overdue_delivered_batch() {
    let _guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    reset_auto_complete_state(&pool).await;
    let (_l1, l2) = seed_customer(&pool).await;
    let (_part_id, batch_id) = seed_delivered_batch(&pool, l2, "AC-OLD", 30).await; // 30 天前 ON_SHELF

    let state = test_state_with_disabled_session(pool.clone());
    run_once(&state, 7).await.expect("run_once");

    let status: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("re-read batch");
    assert_eq!(status, "COMPLETED", "30 天前 DELIVERED 应被 auto_complete 翻 COMPLETED");
}

/// DELIVERED + placed_at 在阈值内（recent）→ run_once 不动。
#[tokio::test]
async fn run_once_skips_recent_batch() {
    let _guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    reset_auto_complete_state(&pool).await;
    let (_l1, l2) = seed_customer(&pool).await;
    let (_part_id, batch_id) = seed_delivered_batch(&pool, l2, "AC-NEW", 1).await; // 1 天前

    let state = test_state_with_disabled_session(pool.clone());
    run_once(&state, 7).await.expect("run_once");

    let status: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("re-read batch");
    assert_eq!(
        status, "DELIVERED",
        "1 天前 DELIVERED 不应被 7 天阈值的 auto_complete 翻"
    );
}

/// commit 后 ws_hub 收到 PART_COMPLETED 事件。
#[tokio::test]
async fn run_once_emits_ws_event_after_commit() {
    let _guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    reset_auto_complete_state(&pool).await;
    let (_l1, l2) = seed_customer(&pool).await;
    let (part_id, _batch_id) = seed_delivered_batch(&pool, l2, "AC-WS", 30).await;

    let state = test_state_with_disabled_session(pool.clone());
    let mut rx = state.ws_hub.subscribe();
    run_once(&state, 7).await.expect("run_once");

    // ws_hub 是 broadcast，timeout 内收到 PART_COMPLETED 即说明 commit 后推送
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut found = false;
    while std::time::Instant::now() < deadline {
        match rx.try_recv() {
            Ok(WsEvent::DashboardEvent { kind, .. }) if kind == "PART_COMPLETED" => {
                found = true;
                break;
            }
            Ok(_) => continue,
            Err(TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            Err(_) => break,
        }
    }
    assert!(found, "ws_hub 应在 commit 后收到 PART_COMPLETED 事件 (part_id={part_id})");
}
