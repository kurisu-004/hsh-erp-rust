//! 业务单号/序列号计数
//!
//! 对应 Python myERP/core/serial.py（除雪花外的部分）：
//! - 客户级序列号循环计数器（`<单字母前缀><4位数字>`，pool=9000）—— 骨架占位
//! - 送货单每日单号计数器
//!
//! 实现要点：
//! - 每日单号计数器用 `INSERT ... ON CONFLICT ... RETURNING` 原子递增
//!   （PG 行级锁天然并发安全，与 Python `repository/delivery_note.py::acquire_no` 对齐）。
//! - `customer_id` 当前未在 SQL 中使用（Python 也是 per-day 全局），但保留参数以备
//!   未来 per-customer counter（与 `t_delivery_note_counter.date_ymd + customer_id`
//!   PK 切换时无需改 service 层）。
//! - 时区归属 Asia/Shanghai，与 Python `core.time.today_yyyymmdd()` 对齐。

use chrono::NaiveDate;
use sqlx::PgExecutor;

use crate::infra::clock::now_naive;
use crate::shared::error::{code, AppError};

/// 客户级循环序列号。骨架阶段返回未实现错误。
pub fn next_customer_serial(_customer_id: i64, _prefix: &str) -> Result<String, AppError> {
    unimplemented!("实施阶段实现客户级序列号循环")
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

/// 业务码给 service 层做 `unreachable!` / 占位时复用（与其它模块错码对齐）。
#[allow(dead_code)]
const PLACEHOLDER_ERR_CODE: i32 = code::INTERNAL;

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