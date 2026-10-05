//! part 域：待编程一览 + 批次列表
//!
//! - `list_pending_programming` —— `GET /parts/pending-programming`，基于
//!   `t_process.is_cnc` 的待编程一览
//! - `list_batches` —— `GET /parts/{part_id}/batches`，某 part 的全部活跃批次
//!
//! 2026-10-02：上架 / 召回（`place_on_shelf` / `recall_to_pending`）随批次用例迁往
//! `crate::modules::prod::batch::service::shelf`，两条 list 端点留在 part 域
//! （它们的对象是 part，不是单个批次）。

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::repo::{PartRepoTrait, PendingProgrammingFilters};
use crate::modules::part::vo::{PartBatchListItemOut, PartListItem, PartListOut};
use crate::shared::error::{AppError, code};

use super::super::PartService;
use super::BatchListRow;
use crate::modules::part::dto_crud::PendingProgrammingQuery;

impl PartService {
    // ===== 1.1 上架 / 召回 =====

    /// `GET /parts/pending-programming`：基于 `t_process.is_cnc` 的待编程一览。
    ///
    /// 2026-09-29 改造：
    /// - 谓词集：`(PENDING | IN_PROCESS | PROGRAMMING)` × 链上含 CNC step 或
    ///   批次在 CNC 货架
    /// - 新 query 参数 `has_cnc_program?: bool`（Tab 切换）：
    ///   - `Some(true)` 仅已上传 G_CODE
    ///   - `Some(false)` 仅未上传
    ///   - `None` 全部
    /// - 出参 `PartListItem` 新增 `has_cnc_program: bool`（由 repo EXISTS 派生）
    ///
    /// 旧实现（status=PROGRAMMING 一览）已废弃，参见 commit 历史。
    pub async fn list_pending_programming<R: PartRepoTrait>(
        mut repo: R,
        query: &PendingProgrammingQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 500);
        let offset = query.offset.unwrap_or(0).max(0);
        let sort_by = match query.sort_by.as_deref().unwrap_or("PLANNED_DELIVERY_DATE") {
            "CREATED_AT" => "created_at",
            "UPDATED_AT" => "updated_at",
            "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
            "REQUEST_DATE" => "request_date",
            "SERIAL_NO" => "serial_no",
            "DRAWING_NO" => "drawing_no",
            "NAME" => "name",
            _ => "planned_delivery_date",
        };
        let sort_dir = query.sort_dir.as_deref().unwrap_or("ASC");
        let f = PendingProgrammingFilters {
            keyword: query.keyword.clone(),
            sort_by: sort_by.to_string(),
            sort_dir: sort_dir.to_string(),
            limit,
            offset,
            has_cnc_program: query.has_cnc_program,
        };
        let items = repo.list_pending_programming_with_cnc_filter(&f).await?;
        let total = repo.count_pending_programming_with_cnc_filter(&f).await?;
        let list_items: Vec<PartListItem> = items
            .into_iter()
            .map(|item| PartListItem {
                has_cnc_program: item.has_cnc_program,
                ..PartListItem::from(item.part)
            })
            .collect();
        Ok(PartListOut {
            items: list_items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/{id}/batches`：工单全部活跃批次。
    ///
    /// 2026-09-30 Phase 2 dashboard 二次调整：扩展 PartBatchListItemOut 7 字段
    /// （`part_id` / `batch_label` / `current_holder_display` / `current_process_step_id` /
    /// `next_process_name` / `delivery_note_no` / `created_at` / `updated_at`），
    /// 修复前端 dashboard PartPreviewDialog Zod 校验 `received undefined` 报错。
    pub async fn list_batches<R: PartRepoTrait>(
        mut repo: R,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<PartBatchListItemOut>, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let _ = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let rows: Vec<BatchListRow> = sqlx::query_as::<_, BatchListRow>(
            // 2026-09-30（rebase 冲突解决）：`next_process_id` / `next_process_name`
            // **继续**从 `current_process_step_id` 经 `LEFT JOIN t_process_chain_step
            // s2` 派生，`s2` JOIN 保留、`t_process p2` 继续挂 `s2.process_id`。
            //
            // 本分支 6ecdf2c 一度把两列改直读 `b.current_process_id`（migration 004），
            // 本冲突处**不采纳**该改法，理由与 2026-09-30 review 第 3 轮 M3 对
            // `GET /prod/batches/inspection` 的回退完全同形：
            //   - 本查询 WHERE **无 status 过滤**，会同时返回 PENDING / IN_PROCESS /
            //     INSPECTION / READY_TO_SHIP 等各状态批次（返修中的批次按
            //     IN_PROCESS 一并返回）；
            //   - 而所有进 INSPECTION 的写点都按「出池 → `current_process_id = NULL`」
            //     不变式把该列清空（`BatchService::scan_inspect` /
            //     `BatchService::receive_from_outsource_to_inspection` /
            //     `BatchService::complete_repair` / `mark_batch_inspected`）→
            //     直读会让 INSPECTION 批次的 `next_process_id` /
            //     `next_process_name` **恒为 null**。
            //
            // 读取方分工（勿越界）：`current_process_id` 的读取方严格限定为 5 条
            // 工序池 SQL + `list_pickable_by_work_type` + rollup 派生；
            // **展示类列表一律走 step 派生**。该清单目前登记了 4 个读点（3 条走
            // step 派生 + 1 个有意例外），**第 4 个例外见
            // `prod/batch/model.rs` 模块 doc 的读取方分工清单**。
            //
            // 2026-09-30 Phase 2（master 6be7531）：投影同时补齐 `b.part_id` /
            // `b.created_at` / `b.updated_at` / `b.current_process_step_id`（修复原
            // DTO 漏投 bug）+ `p2.name` / `dn.delivery_note_no` 两处 LEFT JOIN。
            "SELECT b.id, b.part_id, b.batch_no, b.quantity, b.status, b.is_repairing, \
             b.location, b.current_holder_id, b.current_process_step_id, \
             b.delivery_note_id, b.parent_batch_id, \
             b.created_at, b.updated_at, b.version, \
             COALESCE(s.name, w.name, oc.name) AS current_holder_display, \
             s2.process_id AS next_process_id, \
             p2.name AS next_process_name, \
             dn.delivery_note_no AS delivery_note_no \
             FROM t_part_batch b \
             LEFT JOIN t_shelf s ON s.id = b.current_holder_id \
             LEFT JOIN t_worker w ON w.id = b.current_holder_id \
             LEFT JOIN t_outsource_company oc ON oc.id = b.current_holder_id \
             LEFT JOIN t_process_chain_step s2 ON s2.id = b.current_process_step_id \
             LEFT JOIN t_process p2 ON p2.id = s2.process_id \
             LEFT JOIN t_delivery_note dn ON dn.id = b.delivery_note_id \
             WHERE b.part_id = $1 AND b.deleted_at IS NULL \
             ORDER BY b.batch_no ASC",
        )
        .bind(part_id)
        .fetch_all(repo.conn_mut())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PartBatchListItemOut {
                id: r.id,
                part_id: r.part_id,
                batch_no: r.batch_no,
                // 2026-09-30 Phase 2：沿 delivery_note L{id} 命名风格在 mapper 派生
                batch_label: format!("L{}", r.id),
                quantity: r.quantity,
                status: r.status,
                // 2026-10-01 review 第 1 轮 M5：REPAIRING 已降级为标记列，
                // 列表必须投出它，否则前端拿不到「返修中」信号
                is_repairing: r.is_repairing,
                location: r.location,
                current_holder_id: r.current_holder_id,
                // 2026-09-30 Phase 2：holder_name 重命名为 current_holder_display
                current_holder_display: r.current_holder_display,
                // 2026-09-30 Phase 2：SQL 已选该字段，原 DTO 漏投
                current_process_step_id: r.current_process_step_id,
                // 2026-09-16 PR-3 批次 step 化：next_process_id 由 step.process_id 派生；
                // 2026-09-30 rebase 冲突解决复核后**维持 step 派生**（不直读
                // b.current_process_id），理由见上方 SQL 注释（review 第 3 轮 M3 同形）。
                next_process_id: r.next_process_id,
                // 2026-09-30 Phase 2：LEFT JOIN t_process 派生
                next_process_name: r.next_process_name,
                delivery_note_id: r.delivery_note_id,
                // 2026-09-30 Phase 2：LEFT JOIN t_delivery_note 派生
                delivery_note_no: r.delivery_note_no,
                parent_batch_id: r.parent_batch_id,
                // 2026-09-30 Phase 2：t_part_batch 时间戳
                created_at: r.created_at,
                updated_at: r.updated_at,
                version: r.version,
            })
            .collect())
    }
}
