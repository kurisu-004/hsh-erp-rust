//! Phase 1 / 1.1 上架 / 召回 + 1.5 批次列表 + pending-programming 列表
//!
//! 方法：`place_on_shelf` / `recall_to_pending` / `list_pending_programming` /
// 列表。
//!
//! 2026-09-22 D-6：从原 `phase1.rs` 按业务动作拆出。共享 helper（`ensure_transition` /
// `validate_shelf_zone` / `require_process_chain` / `mark_batch_with_status_and_meta`）
// 在 `phase1/mod.rs` 同 crate 内可见。helper 签名收 `&mut R: PartRepoTrait`，caller
//! 传 `repo.conn_mut()`（生产 `R = &mut PgConnection`，Rust auto-deref + reborrow）。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::{PartRepoTrait, PendingProgrammingFilters};
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part::vo::{PartBatchListItemOut, PartListItem, PartListOut};
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::shared::error::{AppError, code};

use super::super::super::dto_crud::{
    PendingProgrammingQuery, PlaceOnShelfRequest, RecallToPendingRequest,
};
use super::super::PartService;

use super::{
    BatchListRow, assert_shelf_maps_process, ensure_transition, mark_batch_with_status_and_meta,
    require_process_chain, validate_batch_ownership, validate_shelf_zone,
};

impl PartService {
    // ===== 1.1 上架 / 召回 =====

    /// `POST /parts/{id}/place-on-shelf`：PENDING → IN_PROCESS（PRODUCTION_SHELF）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：
    /// - 入口新增 process_chain 必须性守卫（`BIZ_PROCESS_CHAIN_REQUIRED`）
    /// - `req.next_process_id` 经 `ProcessChainRepo::resolve_step_id_by_process`
    ///   解析为 step_id 写入 `t_part_batch.current_process_step_id`
    pub async fn place_on_shelf<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: PlaceOnShelfRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = repo
            .find_batch_by_id(req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::IN_PROCESS, "place-on-shelf")?;
        // PR-3：part 必须已绑定工艺链
        let chain_id = require_process_chain(repo.conn_mut(), part_id).await?;
        // shelf 校验
        validate_shelf_zone(repo.conn_mut(), req.shelf_id, "PRODUCTION").await?;
        // shelf ↔ process 映射
        assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, req.next_process_id).await?;
        // PR-3：解析 step_id（chain 内 process_id → step_id）
        let step_id = ProcessChainRepo::resolve_step_id_by_process(
            repo.conn_mut(),
            chain_id,
            req.next_process_id,
        )
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                format!(
                    "chain {} 内找不到 process_id={} 的活跃 step",
                    chain_id, req.next_process_id
                ),
            )
        })?;
        // 翻状态
        // 2026-09-30：进池 → current_process_id 写目标工序（池归属权威依据）
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            Some(step_id),
            Some(req.next_process_id),
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // rollup
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        // 事件日志
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "PLACED_ON_SHELF",
            from_status: Some(from.as_str()),
            to_status: Some("IN_PROCESS"),
            batch_id: Some(batch.id),
            quantity: Some(batch.quantity),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.note.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "place-on-shelf 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/recall-to-pending`：ON_SHELF / PROGRAMMING → PENDING。
    pub async fn recall_to_pending<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: RecallToPendingRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = repo
            .find_batch_by_id(req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::PENDING, "recall-to-pending")?;
        // 额外 service 守：IN_PROCESS 时必须有 location=PRODUCTION_SHELF（与 Python 一致）
        if from == PartStatus::IN_PROCESS && batch.location.as_deref() != Some("PRODUCTION_SHELF") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "recall-to-pending: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上",
            ));
        }
        // 翻状态
        // 2026-09-30：出池（转 PENDING）→ current_process_id 置 NULL，
        // 否则 PENDING 批次会混进工序候选池
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "PENDING",
            None,
            None,
            None,
            None,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "RECALLED",
            from_status: Some(from.as_str()),
            to_status: Some("PENDING"),
            batch_id: Some(batch.id),
            quantity: Some(batch.quantity),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.note.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "recall 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

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
            // `GET /parts/inspection-batches` 的回退完全同形：
            //   - 本查询 WHERE **无 status 过滤**，会同时返回 PENDING / IN_PROCESS /
            //     INSPECTION / REPAIRING 等各状态批次；
            //   - 而所有进 INSPECTION 的写点都按「出池 → `current_process_id = NULL`」
            //     不变式把该列清空（`phase1::scan` / `outsource::
            //     receive_to_inspection` / `repair::complete_repair` /
            //     `mark_batch_inspected`）→ 直读会让 INSPECTION 批次的
            //     `next_process_id` / `next_process_name` **恒为 null**。
            //
            // 读取方分工（勿越界）：`current_process_id` 的读取方严格限定为 5 条
            // 工序池 SQL + `list_pickable_by_work_type` + rollup 派生；
            // **展示类列表一律走 step 派生**。
            //
            // 2026-09-30 Phase 2（master 6be7531）：投影同时补齐 `b.part_id` /
            // `b.created_at` / `b.updated_at` / `b.current_process_step_id`（修复原
            // DTO 漏投 bug）+ `p2.name` / `dn.delivery_note_no` 两处 LEFT JOIN。
            "SELECT b.id, b.part_id, b.batch_no, b.quantity, b.status, b.location, \
             b.current_holder_id, b.current_process_step_id, \
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
