//! 跨域序列号派发（2026-09-14 Phase 3 重构）
//!
//! 对应 Python `backend-python/repository/serial_counter.py::acquire_serial`。
//!
//! 本模块是**序列号派发的全仓唯一入口**，两个函数配套使用：
//! - [`prefix_for_customer`]：把客户 id 解析成单字符 prefix（查 `t_customer`）
//! - [`acquire`]：拿 prefix 从 `t_serial_counter` 派一个序列号
//!
//! part 域（`POST /parts` / `POST /parts/batch`）与 assembly 域
//! （`POST /assemblies`）都建单即派发序列号，两域共用这两个函数，不各写一份。
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

/// 2026-10-05 新增：取「L1 客户」的 `serial_prefix` 首字符，供建单派发序列号。
///
/// 序列号前缀是 L1 客户的属性（L2 客户的 `serial_prefix` 恒为 NULL，由
/// customer 域 service 双校验保证），所以入参可以是 L1 也可以是 L2：内层子查询
/// 用 `COALESCE(parent_id, id)` 把 L2 折回它的 L1，外层再取那一行的 prefix。
/// 一条 SQL 走完，不在 Rust 侧分叉。
///
/// `COALESCE` **只折一层**：本函数假定客户树恒为两层（建 L2 时必须有 L1 父行、
/// 不得再挂子节点，都由 customer 域 service 保证，DB 侧**无**约束）。
/// 若真出现 L2 的 `parent_id` 指向另一个 L2（第三层），内层折回的是那个中间 L2
/// 而它的 `serial_prefix` 恒为 NULL ⇒ 本函数报 `20308`（语义上应是层级非法，
/// 但该形态进不来，不另设错误码）。
///
/// 三种失败（都不该被 DB CHECK 之外的脏数据放过，故逐个显式判）：
/// - 目标客户或其 L1 父行不存在 / 已软删 → `20102 BIZ_CUSTOMER_NOT_FOUND`
/// - prefix 为 NULL（L1 客户未配前缀）→ `20308 BIZ_CUSTOMER_NO_SERIAL_PREFIX`
/// - prefix 为空串 / 非 ASCII 大写开头 → `20104 BIZ_INVALID_VALUE`
///   （`ck_t_customer_serial_prefix_uppercase` 已用 `^[A-Z]$` 挡住该形态，
///   这两条分支是对脏数据的兜底，正常写入路径进不来）
///
/// 用 `sqlx::query_scalar`（非 `query_scalar!` 宏）：返回值只有
/// `Option<Option<String>>` 两态（无行 / 有行但 prefix 为 NULL），
/// 宏的编译期校验在这里没有额外收益。
pub async fn prefix_for_customer<'e, E: PgExecutor<'e>>(
    executor: E,
    customer_id: i64,
) -> Result<char, AppError> {
    // 外层 Option = 查无此行（客户或其 L1 父行不存在 / 已软删）；
    // 内层 Option = 行在但 `serial_prefix IS NULL`（L1 未配前缀）。
    let row: Option<Option<String>> = sqlx::query_scalar(
        "SELECT serial_prefix FROM t_customer \
         WHERE id = (SELECT COALESCE(parent_id, id) FROM t_customer \
                     WHERE id = $1 AND deleted_at IS NULL) \
           AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .fetch_optional(executor)
    .await?;
    let prefix = row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("customer {customer_id} 不存在或已软删，无法取 serial_prefix"),
        )
    })?;
    let prefix = prefix.ok_or_else(|| {
        AppError::biz(
            code::BIZ_CUSTOMER_NO_SERIAL_PREFIX,
            format!("customer {customer_id} 的 L1 父客户未配置 serial_prefix"),
        )
    })?;
    let ch = prefix.chars().next().ok_or_else(|| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("customer {customer_id} 的 serial_prefix 为空串"),
        )
    })?;
    if !ch.is_ascii_uppercase() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("customer {customer_id} 的 serial_prefix {ch:?} 不是 A-Z 开头"),
        ));
    }
    Ok(ch)
}

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
