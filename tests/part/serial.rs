//! 客户级序列号派发集成测试
//!
//! 覆盖：
//! 1. `acquire_returns_prefixed_4digit_serial` —— 单次 acquire 返回格式化的 Z1000 风格串；
//! 2. `acquire_two_calls_returns_distinct_serials` —— 连续 acquire 拿不同且递增号；
//! 3. `acquire_unknown_prefix_returns_biz_serial_prefix_unknown` —— 未知 prefix → 20108；
//! 4. `acquire_taken_serial_skips_to_next` —— 已存在活跃 part.serial_no 时回退到下一号；
//! 5. `acquire_skips_completed_part_serial` —— `t_part` 上 `COMPLETED` 的号仍占坑
//!    （判占用谓词必须逐字对齐 `uk_t_part_serial_no`）；
//! 6. `acquire_skips_completed_assembly_serial` —— `t_assembly` 上的号（连 `COMPLETED`
//!    的）同样占坑（`uk_t_assembly_serial_no` 没有 status 谓词）。
//!
//! 集成测试运行需要 `tests/common::test_pool` + `ensure_database_exists`：
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），与其它业务测试
//! 互不干扰（DB 间 schema 完全独立）。

// 2026-09-23 PR13 Phase G：serial 域不走 PartFixture（serial 域独立，仅用 fixture 提供
// 1 个 L1 客户作为 `occupy_serial` 的 customer_id 即可）。保留 `reset_serial_state` /
// `seed_prefix` / `occupy_serial` 为本文件私有 helper（PR-C 末统一迁 test-support）。
// 2026-10-05：全仓唯一派号入口是 `shared::serial::acquire`（4 位号池 + 防撞号循环），
// 本文件改测它。

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::shared::error::code;
use hsh_erp_rust::shared::serial::acquire_via_pool;
use hsh_erp_test_support::*;

/// 清空 `t_serial_counter` + `t_part` + `t_assembly`（acquire 关联的两张占用表），
/// 保证测试隔离。
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
    sqlx::query(
        "DELETE FROM t_assembly \
         WHERE serial_no LIKE 'Z%' OR serial_no LIKE 'Y%' OR serial_no LIKE 'X%'",
    )
    .execute(pool)
    .await
    .expect("clean test assembly serials");
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

/// 读回 `t_serial_counter` 的 counter（钉死「counter = 下一个要发的池内下标」）。
async fn counter_of(pool: &sqlx::PgPool, prefix: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT counter FROM t_serial_counter WHERE prefix = $1")
        .bind(prefix)
        .fetch_one(pool)
        .await
        .expect("查 t_serial_counter.counter")
}

/// 造一条 part 行（`status` 由调用方指定），INSERT 期就占住 `serial_no`。
///
/// 判占用谓词是 `deleted_at IS NULL AND status <> 'CANCELLED'`，所以 `COMPLETED`
/// 的行照样占坑 —— `status` 参数化正是为了让这一点可被测试。
async fn occupy_serial(
    pool: &sqlx::PgPool,
    prefix: &str,
    suffix: i64,
    customer_id: i64,
    status: &str,
) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let serial = format!("{prefix}{suffix:04}");
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, \
         version, created_at, updated_at) \
         VALUES ($1, $2, 'TEST', 'D-001', $3, $4, 'TEST', $5, $5, 1, 0, $6, $6)",
    )
    .bind(part_id)
    .bind(&serial)
    .bind(customer_id)
    .bind(status)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part occupying serial");
}

/// 造一条装配件行，INSERT 期就占住 `serial_no`。
///
/// `uk_t_assembly_serial_no` 的谓词是 `deleted_at IS NULL AND serial_no IS NOT NULL`
/// （**无** status 谓词）⇒ 任何未软删的装配件都占坑，`COMPLETED` 也不例外。
async fn occupy_assembly_serial(pool: &sqlx::PgPool, prefix: &str, suffix: i64, status: &str) {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let assembly_id = snowflake.next_id();
    let customer_id = snowflake.next_id();
    let serial = format!("{prefix}{suffix:04}");
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, customer_id, status, \
         request_date, planned_delivery_date, quantity, serial_no, \
         version, created_at, updated_at) \
         VALUES ($1, 'D-ASM-001', 'TEST-ASM', $2, $3, $4, $4, 1, $5, 0, $6, $6)",
    )
    .bind(assembly_id)
    .bind(customer_id)
    .bind(status)
    .bind(today)
    .bind(&serial)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly occupying serial");
}

/// 单次 acquire 返回 `<PREFIX><4位数字>` 格式串。
#[tokio::test]
async fn acquire_returns_prefixed_4digit_serial() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    reset_serial_state(&pool).await;
    seed_prefix(&pool, "Z").await;

    let serial = acquire_via_pool(&pool, 'Z').await.expect("acquire Z");
    assert_eq!(serial, "Z1000", "首次 acquire 应返回 Z + 1000");
    assert_eq!(
        counter_of(&pool, "Z").await,
        1,
        "counter 语义是「下一个要发的池内下标」：发掉 1000 后应为 1"
    );
}

/// 连续两次 acquire 拿不同且递增号。
#[tokio::test]
async fn acquire_two_calls_returns_distinct_serials() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    reset_serial_state(&pool).await;
    seed_prefix(&pool, "Y").await;

    let s1 = acquire_via_pool(&pool, 'Y').await.expect("acquire 1");
    let s2 = acquire_via_pool(&pool, 'Y').await.expect("acquire 2");
    assert_ne!(s1, s2, "连续 acquire 拿不同号");
    assert_eq!(s1, "Y1000");
    assert_eq!(s2, "Y1001");
}

/// 未知 prefix → `BIZ_SERIAL_PREFIX_UNKNOWN`。
#[tokio::test]
async fn acquire_unknown_prefix_returns_biz_serial_prefix_unknown() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    reset_serial_state(&pool).await;
    // 注意：不能 seed "Q9"（单字符限制）
    sqlx::query("DELETE FROM t_serial_counter WHERE prefix = 'Q'")
        .execute(&pool)
        .await
        .expect("clean Q");

    let err = acquire_via_pool(&pool, 'Q')
        .await
        .expect_err("should fail for unknown prefix");
    assert_eq!(err.code(), code::BIZ_SERIAL_PREFIX_UNKNOWN);
}

/// 已存在活跃 serial 时，acquire 跳过占用的号返回下一个空号。
#[tokio::test]
async fn acquire_taken_serial_skips_to_next() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    reset_serial_state(&pool).await;
    seed_prefix(&pool, "X").await;

    // 占 X1000；acquire 应跳过 X1000，返回 X1001
    let dummy_cid = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    occupy_serial(&pool, "X", 1000, dummy_cid, "PENDING").await;

    let serial = acquire_via_pool(&pool, 'X').await.expect("acquire X");
    assert_eq!(
        serial, "X1001",
        "X1000 已占，应跳过到 X1001（counter_for(0)=1000 撞，counter_for(1)=1001 命中）"
    );
}

/// 2026-10-05 新增：`t_part` 上 `COMPLETED` 的号**仍占坑**，必须被跳过。
///
/// `uk_t_part_serial_no` 的谓词是 `serial_no IS NOT NULL AND deleted_at IS NULL
/// AND status <> 'CANCELLED'` —— 只排除软删与 `CANCELLED`，`COMPLETED` 的行还占着号。
/// 判占用谓词若写成 `status NOT IN ('COMPLETED', 'CANCELLED')`（比索引宽 ⇒ 把
/// `COMPLETED` 当空号），acquire 会把 `X1000` 重新发出去，随后建单 INSERT 直接撞
/// 唯一索引报 23505。第二个断言（拿派到的号真去 INSERT）就是钉死这个 23505：
/// 只断言 acquire 的返回值不足以证明「发出去的号一定插得进去」。
#[tokio::test]
async fn acquire_skips_completed_part_serial() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    reset_serial_state(&pool).await;
    seed_prefix(&pool, "X").await;

    let dummy_cid = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    occupy_serial(&pool, "X", 1000, dummy_cid, "COMPLETED").await;

    let serial = acquire_via_pool(&pool, 'X').await.expect("acquire X");
    assert_eq!(
        serial, "X1001",
        "X1000 被一条 COMPLETED 的 part 占着（uk_t_part_serial_no 不排除 COMPLETED），\
         应跳过到 X1001"
    );
    assert_eq!(counter_of(&pool, "X").await, 2, "跳过一次 ⇒ counter 落到 2");

    // 拿派到的号真插一行：谓词写宽时这里会撞 uk_t_part_serial_no（23505）
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, \
         version, created_at, updated_at) \
         VALUES ($1, $2, 'NEW', 'D-NEW', $3, 'PENDING', 'TEST', $4, $4, 1, 0, $5, $5)",
    )
    .bind(snowflake.next_id())
    .bind(&serial)
    .bind(dummy_cid)
    .bind(now.date())
    .bind(now)
    .execute(&pool)
    .await
    .expect("派到的号必须插得进 t_part（撞 uk_t_part_serial_no 说明判占用谓词写宽了）");
}

/// 2026-10-05 新增：`t_assembly` 上的号（连 `COMPLETED` 的）**仍占坑**，必须被跳过。
///
/// `uk_t_assembly_serial_no` 的谓词是 `deleted_at IS NULL AND serial_no IS NOT
/// NULL` —— **没有** status 谓词，所以哪怕装配件已经 `COMPLETED` 也还占着号。part
/// 与 assembly 共用同一个 counter 行与同一个 9000 号池，碰撞检查漏查
/// `t_assembly` 就会发出对方占着的号，INSERT 时撞唯一索引报 23505（生产库里
/// `COMPLETED` 的 4 位装配件号已有数十条，号池回绕后必踩）。
#[tokio::test]
async fn acquire_skips_completed_assembly_serial() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    reset_serial_state(&pool).await;
    seed_prefix(&pool, "X").await;

    occupy_assembly_serial(&pool, "X", 1000, "COMPLETED").await;

    let serial = acquire_via_pool(&pool, 'X').await.expect("acquire X");
    assert_eq!(
        serial, "X1001",
        "X1000 被一条 COMPLETED 的装配件占着（uk_t_assembly_serial_no 无 status 谓词），\
         应跳过到 X1001"
    );
    assert_eq!(counter_of(&pool, "X").await, 2, "跳过一次 ⇒ counter 落到 2");

    // 拿派到的号真插一行：漏查 t_assembly 时这里会撞 uk_t_assembly_serial_no（23505）
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, customer_id, status, \
         request_date, planned_delivery_date, quantity, serial_no, \
         version, created_at, updated_at) \
         VALUES ($1, 'D-ASM-NEW', 'NEW-ASM', $2, 'PENDING', $3, $3, 1, $4, 0, $5, $5)",
    )
    .bind(snowflake.next_id())
    .bind(snowflake.next_id())
    .bind(now.date())
    .bind(&serial)
    .bind(now)
    .execute(&pool)
    .await
    .expect("派到的号必须插得进 t_assembly（撞 uk_t_assembly_serial_no 说明漏查了装配件）");
}
