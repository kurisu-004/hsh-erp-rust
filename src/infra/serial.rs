//! 业务单号/序列号计数
//!
//! 对应 Python myERP/core/serial.py（除雪花外的部分）：
//! - 送货单每日单号计数器
//!
//! 实现要点：
//! - 每日单号计数器用 `INSERT ... ON CONFLICT ... RETURNING` 原子递增
//!   （PG 行级锁天然并发安全，与 Python `repository/delivery_note.py::acquire_no` 对齐）。
//! - `customer_id` 当前未在 SQL 中使用（Python 也是 per-day 全局），但保留参数以备
//!   未来 per-customer counter（与 `t_delivery_note_counter.date_ymd + customer_id`
//!   PK 切换时无需改 service 层）。
//! - 时区归属 Asia/Shanghai，与 Python `core.time.today_yyyymmdd()` 对齐。
//!
//! 客户级 4 位序列号（`{prefix}{4 位数字}`，pool=9000，wrap-around）由
//! `shared::serial::acquire` 实现 —— 它是全仓唯一的派号入口，part 与 assembly
//! 两域共用同一个 counter 行。

use chrono::{Duration, NaiveDate};
use sqlx::PgExecutor;

use crate::infra::clock::now_naive;
use crate::shared::error::AppError;

/// 持续时长阈值 → NaiveDateTime（与 Python `timedelta(days=N)` 对齐）。
#[allow(dead_code)]
pub fn threshold_naive(days: i64) -> chrono::NaiveDateTime {
    now_naive() - Duration::days(days)
}

/// 送货单当日单号（`DN-YYYYMMDD-NNNN`，NN 从 `t_delivery_note_counter` 原子递增）。
///
/// 流程：
/// 1. 取上海当日 `YYYYMMDD`；
/// 2. `INSERT ... ON CONFLICT (date_ymd) DO UPDATE SET last_value = last_value + 1 RETURNING last_value`
///    在 PG 行级锁内拿到下一个 NN（≥1）；
/// 3. 拼装 `DN-{YYYYMMDD}-{NNNN}`。
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
}
