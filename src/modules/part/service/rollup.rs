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
//!
//! （**以上 2026-09-30 起全部作废，见下条**）
//!
//! 2026-09-30 改直读 `t_part_batch.current_process_id`（migration 004）：
//! 派生源换成批次所属工序的新权威列，**删掉整块 step_id → process_id 转译
//! SELECT**。原先的转译有两个问题：
//! 1. 每次 rollup 多一次 DB 往返（batch 状态每变一次就多一次）；
//! 2. 「最慢批次」只按 status 挑、不看工序，若它 `current_process_step_id`
//!    为 NULL（无工序链工单的常态）就会把整个工单的 `t_part.next_process_id`
//!    抹成 NULL —— 而该列是删工序的保护条件之一，等于防线静默失效。
//!
//! 2026-09-22 D-6 重构：方法签名 `<R: PartRepoTrait>`（by-value；trait 已直接
//! `impl for &mut PgConnection`）。
//!
//! ============================================================================
//! 2026-10-01：本文件收敛为「只做派生」的单行委托
//! ============================================================================
//!
//! 派生逻辑（步骤 1–5 + 终态序列号归档 / 释放）已下沉到
//! [`crate::modules::part::repo::status_gate::rollup_part_derived`]，与
//! 「写状态」合成同一个函数 `status_gate::apply_batch_status_change`。
//! 本方法保留为**纯派生入口**：
//!
//! - 新写路径一律经 status_gate（写 + 派生一体，caller 无「要不要调 sync」
//!   这个选项）；本方法只剩历史调用方在用。
//! - 两条路径共用同一段实现，因此行为**完全一致** —— 经 status_gate 写完
//!   状态后再调本方法是安全的冗余（第二次 target == 当前 → NoChange），
//!   不会出现两次释放序列号。
//! - 未来（独立一轮）把剩余的「写完再调 sync」调用点删干净后，本方法即可
//!   删除；本轮不删，避免与并行的 REPAIRING 下游消费方改造抢同一批文件。
//!
//! 2026-10-01 备注：D-6 时代遗留的「inline sqlx 查询（`t_process_chain_step`
//! 不属于 PartRepoTrait 范围）经 `repo.conn_mut()` 走」在本文件已无实际调用点
//! —— 那是 step_id → process_id 转译 SELECT 用的，2026-09-30 随转译一起删除。

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::modules::assembly::service::SyncOutcome;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::repo::status_gate;
use crate::shared::error::AppError;

use super::PartService;

impl PartService {
    /// batch 集变化 → 回流 part 物化列 + 级联 assembly rollup。
    ///
    /// **2026-10-01 起本方法是纯派生入口**：实现一行委托
    /// `status_gate::rollup_part_derived`（与 status_gate 的 step 2–5 同一段
    /// 代码）。新写点请直接用
    /// `status_gate::apply_batch_status_change`（写 + 派生一体）。
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
    /// 2026-09-30 改直读 `current_process_id`（migration 004）：随实现下沉到
    /// `status_gate::rollup_part_derived`。
    ///
    /// 签名收 `&mut R: PartRepoTrait`（而非 `R` by-value）——本方法是 service 层
    /// helper（lifecycle / worker_scan 在 mid-method 调用后仍需继续用 repo），不
    /// 对 handler 暴露。caller 借 `&mut repo` 传入即可继续使用。
    pub async fn sync_from_batch_change<R: PartRepoTrait>(
        repo: &mut R,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<SyncOutcome, AppError> {
        let outcome =
            status_gate::rollup_part_derived(repo.conn_mut(), part_id, current.id).await?;
        Ok(outcome.sync)
    }

    /// 跨域 / 旧路径兼容入口（`conn: &mut PgConnection` → `<&mut PgConnection as PartRepoTrait>`）。
    ///
    /// 由 worker_pool / delivery_note / delivery_group 等**非 part 域** service 调用；
    /// 这些域内部 service 签名仍是 `&mut PgConnection` 直传，没有 `repo: R` 借位。
    /// 通过此薄壳手动指定 `<&mut PgConnection>` 实例化 trait 泛型，避免外部 caller
    /// 写 `&mut &mut *conn` 这种双层 deref。
    ///
    /// part 域内部 lifecycle / worker_scan / phase1 全部走主入口（`repo: &mut R`），
    /// 借 `&mut *tx` 继续使用同一 tx 即可，无需本壳。
    pub async fn sync_from_batch_change_with_conn(
        conn: &mut PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<SyncOutcome, AppError> {
        let outcome = status_gate::rollup_part_derived(conn, part_id, current.id).await?;
        Ok(outcome.sync)
    }
}
