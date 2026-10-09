//! `t_part_event` 的 INSERT 真源（2026-10-12 自 `modules::part::repo::sql::event_sql`
//! 迁入）。
//!
//! ## 为什么是自由函数而不是 ZST 静态方法
//!
//! 老形态是 part 域 ZST 上的事件 INSERT 静态方法 —— 那个 ZST 是
//! `part::repo::sql::PartRepo`（本表在 `part` 域时的归属）。表上提到 shared 之后
//! ZST 不再合法（`shared` 不能反向依赖 `modules::part`），而为一个只做 INSERT 的
//! 单表另起 ZST 又与 `shared::shelf` 的自由函数形态不一致 ⇒ 取自由函数 `insert`。
//!
//! ## 形状不变的两处
//!
//! - 入参仍是**泛型 executor**（`E: PgExecutor<'e>`）而不是 `&mut PgConnection`：
//!   30 个 `insert()` 调用点里有的传 `&mut *conn`、有的传 `repo.conn_mut()`，泛型形态
//!   两边都收。（「30」是 `insert()` 调用点数 = 29 处直接构造 + `prod::queue`
//!   转发器 `part_insert_part_event` 体内那一处；数的时候按 `insert()` 调用点数，不是
//!   builder 字面量数 —— `rg "NewPartEvent {"` 直接得 **30**，因为转发器体内那处
//!   `NewPartEvent` 字面量也算在内。）
//! - `created_at` 仍是**字面量 `now()`**、不是 bind：它不在
//!   [`crate::shared::part_event::model::NewPartEvent`] 里（builder 是 15 列的子集），
//!   改成 bind 就得给 builder 加一个永远没人填的字段。

use sqlx::PgExecutor;

use crate::shared::part_event::model::NewPartEvent;

/// 插入 `t_part_event` 事件日志。
///
/// `id` 由 caller 用 `SnowflakeIdGenerator::next_id()` 预生成；
/// `created_at` 走 DB 默认 `now()`。
///
/// 2026-10-12：列清单加 `worker_id`（`t_worker.id`）与 `process_id`
/// （事件时刻的工序快照，migration 009）两列 —— 此前列清单里根本没有这两列，
/// Rust 写侧恒不写，statistics 域按 `worker_id` 的聚合一直只命中历史零星行。
/// 15 列中实际 bind 13 列（`created_at` 走字面量、`process_id` 与 `worker_id` 见上）。
pub async fn insert<'e, E: PgExecutor<'e>>(
    executor: E,
    e: NewPartEvent<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO t_part_event (
            id, part_id, event_type, from_status, to_status,
            batch_id, quantity, drawing_code, badge_code,
            worker_id, process_id, note, created_at, created_by
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now(), $13
        )
        "#,
        e.id,
        e.part_id,
        e.event_type,
        e.from_status,
        e.to_status,
        e.batch_id,
        e.quantity,
        e.drawing_code,
        e.badge_code,
        e.worker_id,
        e.process_id,
        e.note,
        e.created_by,
    )
    .execute(executor)
    .await?;
    Ok(())
}
