//! part 域 batch → part → assembly 链路 rollup 核心。
//!
//! part/assembly/batch 重构方案 §4.2 (PR-B2)：所有 batch 状态翻转端点不再直接
//! 写 `t_part` 派生列，统一改走 `PartService::sync_from_batch_change`：
//!
//! 1. 拉 part 的全部活跃批次（`deleted_at IS NULL`）
//! 2. 走 `compute_part_target` 算 target status + 派生列
//! 3. target == 当前 → NoChange（不写库、不触发 assembly sync）
//! 4. 内部 UPDATE `t_part`（派生写不走 OCC 冲突，仍 `version += 1` + 写
//!    `updated_by` / `updated_at`）
//! 5. 若 part.status 实际变化 → 调 `AssemblyService::sync_from_part_change`
//!    闭合 batch → part → assembly 链路；返回 `SyncOutcome::Changed(part_id)`
//!    由 handler 决定 WS 广播。
//!
//! 实施约定：本文件只承载 `PartService::sync_from_batch_change`（impl 块拆文件，
//! Rust 允许同 `impl PartService { ... }` 分布在多个同 crate 文件中）。
//!
//! 2026-09-16 PR-3 批次 step 化（migration 028）：rollup 派生规则扩展：
//! - `t_part.next_process_id` 仍保留作派生缓存（PR-2 不动）
//! - 派生源改为：min-progress 活跃 batch.current_process_step_id
//!   → LEFT JOIN t_process_chain_step s ON s.id = step_id → s.process_id
//! - 派生列实际写入：service 层用 step JOIN 取 process_id 后写入 t_part.next_process_id
//! - 多个 batch 共享同一 step 时去重（典型场景：拆分前的同一 step 上下文）

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::modules::assembly::service::{AssemblyService, SyncOutcome};
use crate::modules::part::repo::PartRepo;
use crate::modules::part::statemachine::{compute_part_target, BatchForRollup};
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::shared::error::{code, AppError};

use super::PartService;

impl PartService {
    /// batch 集变化 → 回流 part 物化列 + 级联 assembly rollup（PR-B2 核心）。
    ///
    /// 与 `AssemblyService::sync_from_part_change` 同构，但本方法聚合的是
    /// `t_part_batch.status`（part 与 batch 状态词汇相同，直接取字符串）。
    ///
    /// 错误码：
    /// - 20101 `BIZ_PART_NOT_FOUND` —— part 不存在或已软删
    /// - 5xxxx 系统错 —— sqlx / 内部错误
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：rollup 只物化 `status` +
    /// `next_process_id`（t_part 保留的两列读缓存）；`location` /
    /// `current_holder_id` / `placed_at` 真相源在 t_part_batch，列表页
    /// 按需另查（见 `PartService::list_parts` enrichment）。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：
    /// - `BatchForRollup.next_process_id` → `current_process_step_id`
    /// - 本方法在写入 `t_part.next_process_id` 缓存前一步：取 min-progress
    ///   批次的 step_id，经 `t_process_chain_step.process_id` 派生后写入
    /// - step_id 与 process_id 1:1 对应（同 chain 内 step.process_id 唯一），
    ///   故纯函数 `compute_part_target` 搬运 step_id 再做语义对齐；
    ///   实际写入时已转回 process_id（caller 透传）
    pub async fn sync_from_batch_change(
        conn: &mut PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<SyncOutcome, AppError> {
        // 1. 拉 part 全部活跃批次（rollup 只看活跃行）。
        let batches = PartBatchRepo::list_active_by_part_id(&mut *conn, part_id).await?;

        // 2. 投影到 `BatchForRollup`（仅 rollup 所需 4 列；避免引入完整
        //    `TPartBatch` 让纯函数测试受阻）。
        //
        //    2026-09-16 PR-3 批次 step 化：删 placed_at，next_process_id 改为
        //    current_process_step_id（语义对齐：service 层在写入 t_part 时再
        //    经 step JOIN 转回 process_id）。
        let rows: Vec<BatchForRollup> = batches
            .iter()
            .map(|b| BatchForRollup {
                status: b.status.clone(),
                location: b.location.clone(),
                current_holder_id: b.current_holder_id,
                current_process_step_id: b.current_process_step_id,
            })
            .collect();

        // 3. 空集 → NoChange（防御；正常创建路径不会触发，因为 PR-B1 已保
        //    证每 part 至少有 1 条活跃批次）。
        let Some(target) = compute_part_target(&rows) else {
            return Ok(SyncOutcome::NoChange);
        };

        // 4. 读 part 当前 rollup 状态（status + next_process_id，2 列）。
        //    2026-09-16 PR-2 瘦身：location / current_holder_id / placed_at 列已删。
        let cur = PartRepo::get_part_rollup_state(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
            })?;

        // 5. PR-3 派生：把 target.next_process_id（实际为 step_id）经 step JOIN
        //    转回 process_id，作为 t_part.next_process_id 缓存写入值。
        //
        //    与原 PR-2 行为对齐：当无活跃 step 时返回 NULL（与 part PENDING 时
        //    next_process_id NULL 语义一致）。
        let derived_next_process_id: Option<i64> = if let Some(step_id) = target.next_process_id {
            // 取 step.process_id；step 已软删 / 不存在 → None（防御）
            let row: Option<(i64,)> = sqlx::query_as(
                "SELECT process_id FROM t_process_chain_step \
                 WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(step_id)
            .fetch_optional(&mut *conn)
            .await?;
            row.map(|(pid,)| pid)
        } else {
            None
        };

        // 6. target == 当前 → NoChange（注意：cur.next_process_id 是 process_id，
        //    target.next_process_id 是 step_id 比较后再转换；这里比 process_id）
        if cur.status == target.status && cur.next_process_id == derived_next_process_id {
            return Ok(SyncOutcome::NoChange);
        }

        // 7. 派生写：`WHERE id=$1 AND deleted_at IS NULL`（**不走 OCC 冲突**，
        //    并发 rollup 由 SQL 行锁串行化；version 仍 += 1）。
        let affected = PartRepo::update_part_rollup(
            &mut *conn,
            part_id,
            &target.status,
            derived_next_process_id,
            current.id,
        )
        .await?;
        if affected == 0 {
            // 防御：part 在两次 select 之间被并发软删（极端并发）。整体事务回滚
            // 由 caller 决定 —— 此处返回 NoChange 让 caller 不重试。
            return Ok(SyncOutcome::NoChange);
        }

        // 8. part.status 实际变化 → 调 AssemblyService::sync_from_part_change
        //    闭合链路；返回其 SyncOutcome（可能 Changed/ NoChange）。
        if cur.status != target.status {
            return AssemblyService::sync_from_part_change(&mut *conn, part_id, current).await;
        }
        // status 没变但 next_process_id 物化了 —— 仍算派生写成功，返回
        // Changed(part_id) 供 handler 决定是否广播。
        Ok(SyncOutcome::Changed(part_id))
    }
}