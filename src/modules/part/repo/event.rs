//! `t_part_event` 事件日志
//!
//! 搬迁 `insert_part_event`：service 层在事务内统一插入状态翻转 /
//! 批次拆分 / 返修等事件；本方法只负责 `INSERT`。

use sqlx::PgExecutor;

use crate::modules::part::model::NewPartEvent;

use super::PartRepo;

impl PartRepo {
    /// 插入 `t_part_event` 事件日志。
    ///
    /// `id` 由 caller 用 `SnowflakeIdGenerator::next_id()` 预生成；
    /// `created_at` 走 DB 默认 `now()`。
    pub async fn insert_part_event<'e, E: PgExecutor<'e>>(
        executor: E,
        e: NewPartEvent<'_>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_part_event (
                id, part_id, event_type, from_status, to_status,
                batch_id, quantity, drawing_code, badge_code, note,
                created_at, created_by
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now(), $11
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
            e.note,
            e.created_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }
}