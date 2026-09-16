//! 客户级序列号 `next_customer_serial` 集成测试
//!
//! 覆盖：
//! 1. `acquire_returns_prefixed_4digit_serial` —— 单次 acquire 返回格式化的 F1000 风格串；
//! 2. `acquire_two_calls_returns_distinct_serials` —— 连续 acquire 拿不同且递增号；
//! 3. `acquire_unknown_prefix_returns_biz_serial_prefix_unknown` —— 未知 prefix → 20108；
//! 4. `acquire_taken_serial_skips_to_next` —— 已存在活跃 part.serial_no 时回退到下一号。
//!
//! 集成测试运行需要 `tests/common::test_pool` + `ensure_database_exists`（共享
//! `postgres_rust_test`，与其它业务测试互不干扰）。

#[path = "common/mod.rs"]
mod common;

use common::{ensure_database_exists, test_pool};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::serial::next_customer_serial_via_pool;
use hsh_erp_rust::shared::error::code;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 清空 `t_serial_counter` + `t_part`（acquire 关联表），保证测试隔离。
async fn reset_serial_state(pool: &sqlx::PgPool) {
    sqlx::query("DELETE FROM t_serial_counter WHERE prefix IN ('Z', 'Y', 'X')")
        .execute(pool)
        .await
        .expect("clean test prefixes");
    sqlx::query(
        "DELETE FROM t_part WHERE serial_no LIKE 'Z%' OR serial_no LIKE 'Y%' OR serial_no LIKE 'X%'",
    )
    .execute(pool)
    .await
    .expect("clean test serials");
}

/// seed 测试用 prefix（A-Z 默认由 Python prod_data 注入；测试库独立，需要自 seed）。
async fn seed_prefix(pool: &sqlx::PgPool, prefix: &str) {
    sqlx::query(
        "INSERT INTO t_serial_counter (prefix, counter, version, created_at, updated_at) \
         VALUES ($1, 0, 0, now(), now()) \
         ON CONFLICT (prefix) DO NOTHING",
    )
    .bind(prefix)
    .execute(pool)
    .await
    .expect("seed t_serial_counter");
}

/// 构造一个占用指定 serial_no 的活跃 part 行（status 非 COMPLETED/CANCELLED）。
/// service 层在 acquire 时通过 `t_part` 查重。
async fn occupy_serial(pool: &sqlx::PgPool, prefix: &str, suffix: i64, customer_id: i64) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let serial = format!("{prefix}{suffix:04}");
    let now = now_naive();
    let today = now.date();
    // 2026-09-16 PR-2（migration 027）：t_part 删 `has_been_repaired`；INSERT 列名与
    // VALUES 占位符同步移除 `false` 字面量。
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, \
         version, created_at, updated_at) \
         VALUES ($1, $2, 'TEST', 'D-001', $3, 'PENDING', 'TEST', $4, $4, 1, 0, $5, $5)",
    )
    .bind(part_id)
    .bind(&serial)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert active part");
}

/// 单次 acquire 返回 `<PREFIX><4位数字>` 格式串。
#[tokio::test]
async fn acquire_returns_prefixed_4digit_serial() {
    let _guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    reset_serial_state(&pool).await;
    seed_prefix(&pool, "Z").await;

    let serial = next_customer_serial_via_pool(&pool, 1, "Z").await.expect("acquire Z");
    assert_eq!(serial, "Z1000", "首次 acquire 应返回 Z + 1000");
}

/// 连续两次 acquire 拿不同且递增号。
#[tokio::test]
async fn acquire_two_calls_returns_distinct_serials() {
    let _guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    reset_serial_state(&pool).await;
    seed_prefix(&pool, "Y").await;

    let s1 = next_customer_serial_via_pool(&pool, 1, "Y").await.expect("acquire 1");
    let s2 = next_customer_serial_via_pool(&pool, 1, "Y").await.expect("acquire 2");
    assert_ne!(s1, s2, "连续 acquire 拿不同号");
    assert_eq!(s1, "Y1000");
    assert_eq!(s2, "Y1001");
}

/// 未知 prefix → `BIZ_SERIAL_PREFIX_UNKNOWN`。
#[tokio::test]
async fn acquire_unknown_prefix_returns_biz_serial_prefix_unknown() {
    let _guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    reset_serial_state(&pool).await;
    // 注意：不能 seed "Q9"（单字符限制）
    sqlx::query("DELETE FROM t_serial_counter WHERE prefix = 'Q'")
        .execute(&pool)
        .await
        .expect("clean Q");

    let err = next_customer_serial_via_pool(&pool, 1, "Q")
        .await
        .expect_err("should fail for unknown prefix");
    assert_eq!(err.code(), code::BIZ_SERIAL_PREFIX_UNKNOWN);
}

/// 已存在活跃 serial 时，acquire 跳过占用的号返回下一个空号。
#[tokio::test]
async fn acquire_taken_serial_skips_to_next() {
    let _guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    reset_serial_state(&pool).await;
    seed_prefix(&pool, "X").await;

    // 占 X1000；acquire 应跳过 X1000，返回 X1001
    let dummy_cid = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    occupy_serial(&pool, "X", 1000, dummy_cid).await;

    let serial = next_customer_serial_via_pool(&pool, dummy_cid, "X")
        .await
        .expect("acquire X");
    assert_eq!(
        serial, "X1001",
        "X1000 已占，应跳过到 X1001（counter_for(0)=1000 撞，counter_for(1)=1001 命中）"
    );
}
