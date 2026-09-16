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
    pub async fn sync_from_batch_change(
        conn: &mut PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<SyncOutcome, AppError> {
        // 1. 拉 part 全部活跃批次（rollup 只看活跃行）。
        let batches = PartBatchRepo::list_active_by_part_id(&mut *conn, part_id).await?;

        // 2. 投影到 `BatchForRollup`（仅 rollup 所需 5 列；避免引入完整
        //    `TPartBatch` 让纯函数测试受阻）。
        let rows: Vec<BatchForRollup> = batches
            .iter()
            .map(|b| BatchForRollup {
                status: b.status.clone(),
                location: b.location.clone(),
                current_holder_id: b.current_holder_id,
                next_process_id: b.next_process_id,
                placed_at: b.placed_at,
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

        // 5. target == 当前 → NoChange
        if cur.status == target.status && cur.next_process_id == target.next_process_id {
            return Ok(SyncOutcome::NoChange);
        }

        // 6. 派生写：`WHERE id=$1 AND deleted_at IS NULL`（**不走 OCC 冲突**，
        //    并发 rollup 由 SQL 行锁串行化；version 仍 += 1）。
        let affected = PartRepo::update_part_rollup(
            &mut *conn,
            part_id,
            &target.status,
            target.next_process_id,
            current.id,
        )
        .await?;
        if affected == 0 {
            // 防御：part 在两次 select 之间被软删（极端并发）。整体事务回滚
            // 由 caller 决定 —— 此处返回 NoChange 让 caller 不重试。
            return Ok(SyncOutcome::NoChange);
        }

        // 7. part.status 实际变化 → 调 AssemblyService::sync_from_part_change
        //    闭合链路；返回其 SyncOutcome（可能 Changed/ NoChange）。
        if cur.status != target.status {
            return AssemblyService::sync_from_part_change(&mut *conn, part_id, current).await;
        }
        // status 没变但 next_process_id 物化了 —— 仍算派生写成功，返回
        // Changed(part_id) 供 handler 决定是否广播。
        Ok(SyncOutcome::Changed(part_id))
    }
}
