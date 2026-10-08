//! process_chain 域只读查询
//!
//! 签名：`impl PgExecutor<'_>` —— 同时接受 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//! 读查询一律带 `deleted_at IS NULL`（软删）。

use sqlx::PgExecutor;

use crate::modules::prod::process_chain::model::{TPartProcessChain, TProcessChainStep};

use super::sql::ProcessChainRepo;

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

    /// 2026-10-09 新增：取链内**第一道**未软删 step（`sort_order ASC, id ASC LIMIT 1`）。
    ///
    /// 消费方是 `prod::queue::service::dispatch::dispatch_single`：有工序链的工单
    /// 按**链首**工序下发（2026-10-09 起的不变式，见
    /// `shared::batch::chain` 模块 doc 的背景段）。口径与端点 5
    /// `preview_auto_dispatch` 的 `LEFT JOIN LATERAL … ORDER BY pcs.sort_order ASC,
    /// pcs.id ASC LIMIT 1` 逐条相同 —— 预览说「可下发到首道工序」，写端点就必须真的
    /// 下发到首道，否则「前端照 preview 显示的值操作，落库却是另一道」。
    ///
    /// 返回 `(step_id, process_id)`；`None` = 链内一个未软删 step 都没有（链被软删
    /// 时 `t_part.process_chain_id` 仍是旧 id，会走到这里）。**没有**「链内工序
    /// 歧义」这一返回：取的是链首，与链内 `process_id` 是否重复无关。
    ///
    /// 走运行时 `sqlx::query_as`（与同文件 `resolve_step_id_by_process` 同款）：
    /// 只投影两个列，进 `.sqlx/` 离线缓存带来的维护成本高于收益。
    pub async fn first_step_in_chain<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
    ) -> Result<Option<(i64, i64)>, sqlx::Error> {
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT id, process_id FROM t_process_chain_step \
             WHERE chain_id = $1 AND deleted_at IS NULL \
             ORDER BY sort_order ASC, id ASC \
             LIMIT 1",
        )
        .bind(chain_id)
        .fetch_optional(executor)
        .await?;
        Ok(row)
    }

    /// 2026-09-16 PR-3 批次 step 化：在指定 chain 内按 process_id 解析 step_id。
    ///
    /// 用于：
    /// - place_on_shelf / release_from_programming / 外协发送（`service/move.rs`）等
    ///   进入生产流场景：service 拿到 caller 传的 process_id 后必须解析为
    ///   step_id 才能写入 `t_part_batch.current_process_step_id`
    /// - worker RETURNED / to_process 等"保持当前 step"场景：service 按当前
    ///   step.process_id 反查 step_id（保持语义对齐）
    ///
    /// 返回：
    /// - `Ok(Some(step_id))` —— 唯一匹配（链内同一 process_id 应唯一）
    /// - `Ok(None)` —— chain 内找不到（process_id 不在链中或 step 已软删）
    ///
    /// 注：链内同一 process_id 重复（数据异常）的歧义不在本函数守，
    /// caller 用 `count_steps_by_chain_process` 单独查证。
    pub async fn resolve_step_id_by_process<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
        process_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_process_chain_step \
             WHERE chain_id = $1 AND process_id = $2 AND deleted_at IS NULL \
             LIMIT 1",
        )
        .bind(chain_id)
        .bind(process_id)
        .fetch_optional(executor)
        .await?;
        Ok(row.map(|(sid,)| sid))
    }

    /// 2026-09-16 PR-3 批次 step 化：查链内 (process_id, sort_order > current.sort_order)
    /// 的下一步 step（worker RETURNED / to_process 等"推进到下一步"场景）。
    ///
    /// 入参：当前 step 的 (chain_id, current_sort_order)。
    /// 出参：sort_order 最小的下一步 step；None 表示链已到末端（→ to_inspection）。
    pub async fn next_step_in_chain<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
        current_sort_order: i32,
    ) -> Result<Option<TProcessChainStep>, sqlx::Error> {
        sqlx::query_as!(
            TProcessChainStep,
            r#"
            SELECT id, chain_id, sort_order, process_id, estimated_minutes, note,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_process_chain_step
            WHERE chain_id = $1
              AND sort_order > $2
              AND deleted_at IS NULL
            ORDER BY sort_order ASC
            LIMIT 1
            "#,
            chain_id,
            current_sort_order,
        )
        .fetch_optional(executor)
        .await
    }
}
