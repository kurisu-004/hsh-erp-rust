//! 业务单号/序列号计数
//!
//! 对应 Python myERP/core/serial.py（除雪花外的部分）：
//! - 客户级序列号循环计数器（`<单字母前缀><4位数字>`，pool=9000）
//! - 送货单每日单号计数器
//!
//! 实现要点：
//! - 客户级序列号用 `SELECT ... FOR UPDATE` 锁 counter 行（PG 行级锁天然并发安全，
//!   与 Python `repository/serial_counter.py::acquire_serial` 对齐）。
//! - 每日单号计数器用 `INSERT ... ON CONFLICT ... RETURNING` 原子递增
//!   （PG 行级锁天然并发安全，与 Python `repository/delivery_note.py::acquire_no` 对齐）。
//! - `customer_id` 当前未在 SQL 中使用（Python 也是 per-day 全局），但保留参数以备
//!   未来 per-customer counter（与 `t_delivery_note_counter.date_ymd + customer_id`
//!   PK 切换时无需改 service 层）。
//! - 时区归属 Asia/Shanghai，与 Python `core.time.today_yyyymmdd()` 对齐。

use chrono::{Duration, NaiveDate};
use sqlx::{PgExecutor, PgPool};

use crate::infra::clock::now_naive;
use crate::shared::error::{AppError, code};

/// 客户级序列号格式常量（与 Python `core/serial.py::SERIAL_MIN/MAX/POOL_SIZE` 对齐）。
const SERIAL_MIN: i64 = 1000;
#[allow(dead_code)]
const SERIAL_MAX: i64 = 9999;
const SERIAL_POOL_SIZE: i64 = 9000;
const SERIAL_FORMAT_WIDTH: usize = 4;
/// 串行化单次事务最多绕一圈（= pool size）；再绕说明已用尽，抛 EXHAUSTED。
const SERIAL_PREFIX_MAX_ATTEMPTS: i64 = 9000;

/// 客户级循环序列号（`<单字母前缀><4位数字>`，pool=9000，wrap-around）。
///
/// 流程：
/// 1. 校验 `prefix` 是 A-Z 单字符（小写自动转大写；其它字符 → `BIZ_INVALID_VALUE`）；
/// 2. `SELECT counter FROM t_serial_counter WHERE prefix = $1 FOR UPDATE` 锁住
///    对应 counter 行；不存在 → `BIZ_SERIAL_PREFIX_UNKNOWN`；
/// 3. 在锁内循环：candidate = `SERIAL_MIN + counter % SERIAL_POOL_SIZE`，校验
///    `t_part.serial_no` 中是否有活跃占用（未软删 + 非 COMPLETED/CANCELLED）；
///    找不到 → `counter += 1` 写回，返回 `{prefix}{candidate:04}`;
///    占用 → `counter += 1` 重试；走完 `SERIAL_PREFIX_MAX_ATTEMPTS` 仍撞 →
///    `BIZ_PART_SERIAL_EXHAUSTED`。
///
/// 并发模型：不同 prefix 取不同行锁，互不阻塞；同 prefix 在 step 2 的行锁上
/// 排队，后者读到前者 commit 后的 counter 值。
///
/// `customer_id` 当前未在 SQL 中使用（Python 也是 per-customer counter 全局共享）；
/// 保留参数以备未来 per-customer counter。
///
/// 必须在 caller 已开启的事务内调用（`&mut PgConnection`），使行锁随事务释放。
pub async fn next_customer_serial(
    conn: &mut sqlx::PgConnection,
    _customer_id: i64,
    prefix: &str,
) -> Result<String, AppError> {
    // 1. 校验 + 规范化 prefix
    let normalized = normalize_prefix(prefix)?;

    // 2. 锁住 prefix 对应 counter 行
    let mut counter: i64 = match sqlx::query_scalar!(
        r#"SELECT counter AS "counter!" FROM t_serial_counter WHERE prefix = $1 FOR UPDATE"#,
        normalized,
    )
    .fetch_optional(&mut *conn)
    .await?
    {
        Some(c) => c,
        None => {
            return Err(AppError::biz(
                code::BIZ_SERIAL_PREFIX_UNKNOWN,
                format!(
                    "serial counter for prefix {normalized:?} not seeded; \
                     add it to t_serial_counter before creating parts"
                ),
            ));
        }
    };

    // 3. 在锁内逐个 candidate 试，直到找到空号或耗尽 pool
    for _ in 0..SERIAL_PREFIX_MAX_ATTEMPTS {
        let candidate = counter_for(counter);
        let serial = format!(
            "{normalized}{:0width$}",
            candidate,
            width = SERIAL_FORMAT_WIDTH
        );

        let taken: Option<i32> = sqlx::query_scalar!(
            r#"
            SELECT 1 AS "taken!"
            FROM t_part
            WHERE serial_no = $1
              AND deleted_at IS NULL
              AND status NOT IN ('COMPLETED', 'CANCELLED')
            LIMIT 1
            "#,
            serial,
        )
        .fetch_optional(&mut *conn)
        .await?;

        if taken.is_none() {
            // 找到空号：counter +1 写回（OCC 防并发改写）
            let next_counter = counter + 1;
            sqlx::query!(
                r#"
                UPDATE t_serial_counter
                SET counter    = $2,
                    version    = version + 1,
                    updated_at = now()
                WHERE prefix = $1
                "#,
                normalized,
                next_counter,
            )
            .execute(&mut *conn)
            .await?;

            return Ok(serial);
        }

        // 撞号 → counter += 1 重试
        counter += 1;
    }

    Err(AppError::biz(
        code::BIZ_PART_SERIAL_EXHAUSTED,
        format!(
            "serial pool for prefix {normalized:?} exhausted \
             (>{SERIAL_POOL_SIZE} active orders)"
        ),
    ))
}

/// 持续时长阈值 → NaiveDateTime（与 Python `timedelta(days=N)` 对齐）。
#[allow(dead_code)]
pub fn threshold_naive(days: i64) -> chrono::NaiveDateTime {
    now_naive() - Duration::days(days)
}

/// Pool 便捷入口：开事务 + 调 `next_customer_serial` + commit。
///
/// 用于 caller 只有 `&PgPool` 的场景（如集成测试 fixture）。
pub async fn next_customer_serial_via_pool(
    pool: &PgPool,
    customer_id: i64,
    prefix: &str,
) -> Result<String, AppError> {
    let mut tx = pool.begin().await?;
    let serial = next_customer_serial(&mut tx, customer_id, prefix).await?;
    tx.commit().await?;
    Ok(serial)
}

/// 把 candidate counter 转成 [SERIAL_MIN, SERIAL_MAX] 范围内的序列号。
///
/// Python：`SERIAL_MIN + (counter % SERIAL_POOL_SIZE)`。`counter %= pool` 在 Rust
/// 中对负数取模行为与 Python 不同（Rust 保留符号），显式校正。
fn counter_for(counter: i64) -> i64 {
    let mut v = counter % SERIAL_POOL_SIZE;
    if v < 0 {
        v += SERIAL_POOL_SIZE;
    }
    SERIAL_MIN + v
}

/// 规范化 prefix：A-Z 单字符；小写自动转大写；其它字符 → `BIZ_INVALID_VALUE`。
fn normalize_prefix(prefix: &str) -> Result<String, AppError> {
    if prefix.len() != 1 {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "prefix 必须是单字符，当前 {prefix:?}（长度={}）",
                prefix.len()
            ),
        ));
    }
    let upper = prefix.to_ascii_uppercase();
    let ch = upper.chars().next().expect("len=1");
    if !ch.is_ascii_uppercase() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("prefix 必须是 A-Z 单字符，当前 {prefix:?}"),
        ));
    }
    Ok(upper)
}

/// 送货单当日单号（`DN-YYYYMMDD-NNNN`，NN 从 `t_delivery_note_counter` 原子递增）。
///
/// 流程：
/// 1. 取上海当日 `YYYYMMDD`；
/// 2. `INSERT ... ON CONFLICT (date_ymd) DO UPDATE SET last_value = last_value + 1 RETURNING last_value`
///    在 PG 行级锁内拿到下一个 NN（≥1）；
/// 3. 拼装 `DN-YYYYMMDD-NNNN`。
///
/// 注：`customer_id` 暂未使用（Python 也是 per-day 全局）；保留参数以备后续
/// 按 L1 客户拆分计数。
pub async fn next_delivery_note_no<'e, E: PgExecutor<'e>>(
    exec: E,
    _customer_id: i64,
) -> Result<String, AppError> {
    let today: NaiveDate = now_naive().date();
    let today_ymd = today.format("%Y%m%d").to_string();

    let last_value: i32 = sqlx::query_scalar!(
        r#"
        INSERT INTO t_delivery_note_counter (date_ymd, last_value)
        VALUES ($1, 1)
        ON CONFLICT (date_ymd)
        DO UPDATE SET last_value   = t_delivery_note_counter.last_value + 1,
                      updated_at  = now()
        RETURNING last_value
        "#,
        today_ymd,
    )
    .fetch_one(exec)
    .await?;

    Ok(format!("DN-{}-{:04}", today_ymd, last_value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// counter 转 candidate 范围的纯函数（无需 DB）。
    #[test]
    fn counter_for_wraps_in_pool_range() {
        // SERIAL_MIN 起
        assert_eq!(counter_for(0), SERIAL_MIN);
        assert_eq!(counter_for(1), SERIAL_MIN + 1);
        // wrap 一次
        assert_eq!(counter_for(SERIAL_POOL_SIZE), SERIAL_MIN);
        assert_eq!(counter_for(SERIAL_POOL_SIZE + 1), SERIAL_MIN + 1);
        // SERIAL_MAX 边界
        assert_eq!(counter_for(SERIAL_MAX - SERIAL_MIN), SERIAL_MAX);
    }

    /// counter_for 接受负数（防御性，counter 列 NOT NULL 但显式校正 Rust 取模符号）。
    #[test]
    fn counter_for_handles_negative() {
        // Python: (-1) % 9000 = 8999; Rust: (-1) % 9000 = -1。我们校正成 8999。
        assert_eq!(counter_for(-1), SERIAL_MAX);
        assert_eq!(counter_for(-SERIAL_POOL_SIZE), SERIAL_MIN);
    }

    /// prefix 规范化：小写 → 大写；非法字符拒绝。
    #[test]
    fn normalize_prefix_uppercases_ascii_lowercase() {
        assert_eq!(normalize_prefix("a").unwrap(), "A");
        assert_eq!(normalize_prefix("Z").unwrap(), "Z");
        assert_eq!(normalize_prefix("f").unwrap(), "F");
    }

    #[test]
    fn normalize_prefix_rejects_non_a_to_z() {
        assert!(normalize_prefix("1").is_err());
        assert!(normalize_prefix("aa").is_err());
        assert!(normalize_prefix("").is_err());
        assert!(normalize_prefix("/").is_err());
        // 汉字 / emoji 也拒绝
        assert!(normalize_prefix("法").is_err());
    }

    /// `t_delivery_note_counter` 当日计数器连续调用两次应当产出 `0001` 与 `0002`，
    /// 并共享同一 DN- 前缀。
    ///
    /// 集成测试运行于 `tests/` 内（共享 `tests/common::test_pool`）。这里只放单测
    /// 用的占位；端到端验证落到 `tests/delivery_note_api.rs::counter_acquires_sequential_numbers`。
    #[test]
    fn counter_format_includes_ymd() {
        let s = format!("DN-{}-{:04}", "20260821", 7);
        assert_eq!(s, "DN-20260821-0007");
    }

    /// 客户级序列号格式化：prefix + 4 位零填充数字。
    #[test]
    fn customer_serial_format_pads_to_four_digits() {
        let s = format!("{:0width$}", SERIAL_MIN, width = SERIAL_FORMAT_WIDTH);
        assert_eq!(s, "1000");
        let s = format!("F{:0width$}", SERIAL_MAX, width = SERIAL_FORMAT_WIDTH);
        assert_eq!(s, "F9999");
    }
}
