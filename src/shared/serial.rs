//! 跨域序列号派发（2026-09-14 Phase 3 重构）
//!
//! 对应 Python `backend-python/repository/serial_counter.py::acquire_serial`。
//!
//! ## `acquire(conn, prefix) -> String`
//! 通用派发：原子 `UPDATE t_serial_counter SET counter = counter + 1 RETURNING counter`，
//! 格式 `{prefix}{counter:07}`（counter 从 1 开始），counter >= 99_999_999 视为耗尽。
//!
//! 与 `infra/serial::next_customer_serial` 的差异：
//! - `next_customer_serial`：循环 + 防撞号 + 校验 active part（业务用）
//! - `acquire`：纯原子递增，无循环（assembly / 早期 part 用）
//!
//! Phase 3 把 assembly 域的 `acquire_serial` 内联实现迁移到这里，保留相同语义。

use sqlx::{PgConnection, PgExecutor};

use crate::shared::error::{AppError, code};

/// 从 `t_serial_counter` 派发下一个序列号（`prefix` 是单字符业务 PK）。
///
/// 格式：`{prefix}{counter:07}`（counter 从 1 开始；与 Python
/// `repository/serial_counter.py::acquire_serial` 对齐）。
///
/// `counter >= 99_999_999` 视为耗尽，返回 `BIZ_PART_SERIAL_EXHAUSTED`（20105）。
///
/// `prefix` 必须是 A-Z 单字符（DB CHECK 约束），其它字符或空字符串 → `BIZ_INVALID_VALUE`。
pub async fn acquire(conn: &mut PgConnection, prefix: char) -> Result<String, AppError> {
    let prefix_str = prefix.to_string();
    if !prefix.is_ascii_uppercase() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("prefix 必须是 A-Z 单字符，当前 {prefix:?}"),
        ));
    }
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE t_serial_counter SET counter = counter + 1, updated_at = NOW() \
         WHERE prefix = $1 RETURNING counter",
    )
    .bind(&prefix_str)
    .fetch_optional(&mut *conn)
    .await?;
    let counter = row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_SERIAL_PREFIX_UNKNOWN,
            format!("prefix '{prefix}' 未注册"),
        )
    })?;
    if counter.0 >= 99_999_999 {
        return Err(AppError::biz(
            code::BIZ_PART_SERIAL_EXHAUSTED,
            "序列号池耗尽",
        ));
    }
    Ok(format!("{}{:07}", prefix, counter.0))
}

/// Pool 便捷入口（同 `infra::serial::next_customer_serial_via_pool` 模式）。
///
/// caller 只有 `&sqlx::PgPool` 时（少量集成测试场景）可用；正式 service 流程
/// 必须传 `&mut PgConnection` 以保证事务一致性。
#[allow(dead_code)]
pub async fn acquire_via_pool(pool: &sqlx::PgPool, prefix: char) -> Result<String, AppError> {
    let mut tx = pool.begin().await?;
    let serial = acquire(&mut tx, prefix).await?;
    tx.commit().await?;
    Ok(serial)
}

/// Pool + executor 便捷入口（接受任何 `PgExecutor`）。
///
/// 用于 caller 已有 `&mut Transaction` 或 `&PgPool` 的场景。
#[allow(dead_code)]
pub async fn acquire_via_executor<'e, E: PgExecutor<'e>>(
    exec: E,
    prefix: char,
) -> Result<String, AppError> {
    let prefix_str = prefix.to_string();
    if !prefix.is_ascii_uppercase() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("prefix 必须是 A-Z 单字符，当前 {prefix:?}"),
        ));
    }
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE t_serial_counter SET counter = counter + 1, updated_at = NOW() \
         WHERE prefix = $1 RETURNING counter",
    )
    .bind(&prefix_str)
    .fetch_optional(exec)
    .await?;
    let counter = row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_SERIAL_PREFIX_UNKNOWN,
            format!("prefix '{prefix}' 未注册"),
        )
    })?;
    if counter.0 >= 99_999_999 {
        return Err(AppError::biz(
            code::BIZ_PART_SERIAL_EXHAUSTED,
            "序列号池耗尽",
        ));
    }
    Ok(format!("{}{:07}", prefix, counter.0))
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn format_serial_pads_seven_digits() {
        assert_eq!(format!("F{:07}", 1), "F0000001");
        assert_eq!(format!("A{:07}", 100), "A0000100");
        // 8 位数字本身不需要补 0
        assert_eq!(format!("F{:07}", 10_000_000), "F10000000");
    }
}
