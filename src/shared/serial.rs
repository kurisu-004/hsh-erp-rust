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
//! 4 位号池循环派发：`SELECT ... FOR UPDATE` 锁住 counter 行 → 逐个试
//! `{prefix}{1000 + counter % 9000}` → 空闲则写回 `counter + 1` 并返回，
//! 占用则 `counter + 1` 重试。详见 [`acquire`] 的文档。

use sqlx::{PgConnection, PgExecutor};

use crate::shared::error::{AppError, code};

/// 4 位号池下界（与 Python `core/serial.py::SERIAL_MIN` 对齐）。
const SERIAL_MIN: i64 = 1000;
/// 4 位号池上界。
const SERIAL_MAX: i64 = 9999;
/// 号池宽度（`[1000, 9999]` 共 9000 个号，回绕周期）。
const SERIAL_POOL_SIZE: i64 = 9000;
/// 序列号数字部分宽度（4 位补零）。
const SERIAL_FORMAT_WIDTH: usize = 4;
/// 单次派发最多绕一圈（= 号池宽度）；再绕说明整池占满，抛 `20105`。
const SERIAL_PREFIX_MAX_ATTEMPTS: i64 = SERIAL_POOL_SIZE;

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
/// 2026-10-05 起本函数是**派号链路上 A-Z 校验的唯一一处**：`acquire` 收 `char`
/// 且不重复校验，脏 prefix 必须在这里被挡住（DB CHECK
/// `ck_t_customer_serial_prefix_uppercase` 是更外层的兜底）。
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

/// 从 `t_serial_counter` 派发下一个序列号（4 位号池，`prefix` 是单字符业务 PK）。
///
/// ## 格式与号池
/// `{prefix}{4 位数字}`，数字部分取自 4 位号池 `[1000, 9999]`：
/// `candidate = 1000 + counter % 9000`，`counter` 越过 9000 后**回绕**重新从 1000
/// 开始。号池用尽（整池 9000 个号全部被占用、绕一圈仍找不到空号）→
/// `BIZ_PART_SERIAL_EXHAUSTED`（20105）。
///
/// `prefix` 未在 `t_serial_counter` 注册 → `BIZ_SERIAL_PREFIX_UNKNOWN`（20108）。
/// A-Z 校验不在这里做：由 [`prefix_for_customer`] 独占（脏 prefix 在客户层就被
/// 20104 挡住），本函数只接收它产出的 A-Z 单字符。
///
/// ## `counter` 语义：**先用后递增**
/// `t_serial_counter.counter` 是**下一个要发的池内下标**，不是「已发计数」。
/// `counter = 0` ⇒ 发出 `{prefix}1000` 并把 counter 写成 1；`counter = 1` ⇒ 发出
/// `{prefix}1001`。`version` 随之 +1。
///
/// **counter 不得手工重置**（改成 0 / 改小以「重新从 1000 开始」都是错的）：重置
/// 后碰撞循环要**逐个**跳过已被占用的号，而循环全程持 `t_serial_counter` 该行的
/// `FOR UPDATE` 行锁 ⇒ 同一 prefix 的所有建单在这一次扫描期间全部排队（单次最坏
/// 9000 次探测 × 2 张表的占用查询）。号池回绕本身就是「用完自动回到 1000」的
/// 机制，不需要（也不该）靠重置实现。
///
/// ## part 与 assembly 共用同一个号池
/// `t_part.serial_no` 与 `t_assembly.serial_no` 从 `t_serial_counter` 的**同一行**
/// 取号，因此同一 L1 prefix 下两类单据共用一个序列流、互相插号。碰撞检查也
/// 必须**同时**覆盖两张表（part 与 assembly 各按自己的唯一索引谓词判占用），
/// 少查一张就会发出对方已经占着的号。
///
/// ## 并发模型
/// 不同 prefix 取不同行锁、互不阻塞；同一 prefix 在 step 1 的行锁上排队，后者
/// 读到前者提交后的 counter 值。
///
/// 必须在 caller 已开启的事务内调用（`&mut PgConnection`），使行锁随事务释放、
/// 且整个碰撞循环与后续 INSERT 处在同一个事务里。
pub async fn acquire(conn: &mut PgConnection, prefix: char) -> Result<String, AppError> {
    // 1. 锁住 prefix 对应的 counter 行
    let mut counter: i64 = match sqlx::query_scalar!(
        r#"SELECT counter AS "counter!" FROM t_serial_counter WHERE prefix = $1 FOR UPDATE"#,
        prefix.to_string(),
    )
    .fetch_optional(&mut *conn)
    .await?
    {
        Some(c) => c,
        None => {
            return Err(AppError::biz(
                code::BIZ_SERIAL_PREFIX_UNKNOWN,
                format!(
                    "serial counter for prefix {prefix:?} not seeded; \
                     add it to t_serial_counter before creating parts"
                ),
            ));
        }
    };

    // 2. 在锁内逐个 candidate 试，直到找到空号或号池耗尽
    for _ in 0..SERIAL_PREFIX_MAX_ATTEMPTS {
        let serial = format!(
            "{prefix}{:0width$}",
            counter_for(counter),
            width = SERIAL_FORMAT_WIDTH
        );

        // 占用判定必须逐字对齐各自的唯一索引谓词：判宽了（多看）只是多跳几个空号，
        // 判窄了（少看）会把对方还占着的号当成空号重新发出，直到 INSERT 撞
        // 唯一索引报 23505。
        //   uk_t_part_serial_no     : serial_no IS NOT NULL AND deleted_at IS NULL
        //                            AND status <> 'CANCELLED'
        //   uk_t_assembly_serial_no : serial_no IS NOT NULL AND deleted_at IS NULL
        let part_taken: Option<i32> = sqlx::query_scalar!(
            r#"
            SELECT 1 AS "taken!"
            FROM t_part
            WHERE serial_no = $1
              AND deleted_at IS NULL
              AND status <> 'CANCELLED'
            LIMIT 1
            "#,
            serial,
        )
        .fetch_optional(&mut *conn)
        .await?;
        let assembly_taken: Option<i32> = sqlx::query_scalar!(
            r#"
            SELECT 1 AS "taken!"
            FROM t_assembly
            WHERE serial_no = $1
              AND deleted_at IS NULL
            LIMIT 1
            "#,
            serial,
        )
        .fetch_optional(&mut *conn)
        .await?;

        if part_taken.is_none() && assembly_taken.is_none() {
            // 找到空号：counter +1 写回（counter 语义是「下一个要发的池内下标」）
            sqlx::query!(
                r#"
                UPDATE t_serial_counter
                SET counter    = $2,
                    version    = version + 1,
                    updated_at = now()
                WHERE prefix = $1
                "#,
                prefix.to_string(),
                counter + 1,
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
            "serial pool for prefix {prefix:?} exhausted \
             ([{SERIAL_MIN}, {SERIAL_MAX}] 共 {SERIAL_POOL_SIZE} 个号全部被占用)"
        ),
    ))
}

/// 把 counter 转成号池内 `[SERIAL_MIN, SERIAL_MAX]` 的序列号数字部分。
///
/// 业务口径是 `SERIAL_MIN + (counter % SERIAL_POOL_SIZE)`。Rust 的 `%` 对负数
/// 保留符号（Python 恒为非负），显式校正。
fn counter_for(counter: i64) -> i64 {
    let mut v = counter % SERIAL_POOL_SIZE;
    if v < 0 {
        v += SERIAL_POOL_SIZE;
    }
    SERIAL_MIN + v
}

/// Pool 便捷入口。
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

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    /// counter → 号池内序列号的纯函数（无需 DB）。
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

    /// 序列号格式化：prefix + 4 位零填充数字。
    #[test]
    fn customer_serial_format_pads_to_four_digits() {
        let s = format!("{:0width$}", SERIAL_MIN, width = SERIAL_FORMAT_WIDTH);
        assert_eq!(s, "1000");
        let s = format!("F{:0width$}", SERIAL_MAX, width = SERIAL_FORMAT_WIDTH);
        assert_eq!(s, "F9999");
    }
}
