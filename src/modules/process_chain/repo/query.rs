//! process_chain 域只读查询
//!
//! 签名：`impl PgExecutor<'_>` —— 同时接受 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//! 读查询一律带 `deleted_at IS NULL`（软删）。

use sqlx::PgExecutor;

use crate::modules::process_chain::model::{TPartProcessChain, TProcessChainStep};

use super::ProcessChainRepo;

impl ProcessChainRepo {
    /// 按 `part_id` 取链 header（活跃行，未软删）。
    ///
    /// 2026-09-16 FK 翻转（migration 026）：归属关系改由 `t_part.process_chain_id`
    /// 承载，本查询经 `t_part` JOIN 取链；part 已软删 / 未绑定 → None。
    /// 1:1 binding → 0 行（无链）或 1 行（含软删链视为不存在）。
    pub async fn get_chain_by_part<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<Option<TPartProcessChain>, sqlx::Error> {
        sqlx::query_as!(
            TPartProcessChain,
            r#"
            SELECT c.id, c.name, c.version, c.note,
                   c.created_at, c.created_by, c.updated_at, c.updated_by, c.deleted_at
            FROM t_part_process_chain c
            JOIN t_part p ON p.process_chain_id = c.id
            WHERE p.id = $1 AND p.deleted_at IS NULL AND c.deleted_at IS NULL
            "#,
            part_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 `chain_id` 主键取链 header（活跃行，未软删）。2026-09-16 新增：
    /// 支撑 `GET /process-chains/{chain_id}`（前端点击零件后按链 id 加载工序）。
    pub async fn get_chain_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
    ) -> Result<Option<TPartProcessChain>, sqlx::Error> {
        sqlx::query_as!(
            TPartProcessChain,
            r#"
            SELECT id, name, version, note,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_process_chain
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            chain_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 `chain_id` 列未软删的步骤（按 sort_order 升序）。
    pub async fn list_steps_by_chain<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
    ) -> Result<Vec<TProcessChainStep>, sqlx::Error> {
        sqlx::query_as!(
            TProcessChainStep,
            r#"
            SELECT id, chain_id, sort_order, process_id, estimated_minutes, note,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_process_chain_step
            WHERE chain_id = $1 AND deleted_at IS NULL
            ORDER BY sort_order ASC, id ASC
            "#,
            chain_id,
        )
        .fetch_all(executor)
        .await
    }
}
