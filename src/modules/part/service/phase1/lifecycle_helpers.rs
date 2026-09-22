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
use crate::modules::part::repo::PartListFilters;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part::vo::{PartBatchListItemOut, PartListItem, PartListOut};
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::shared::error::{AppError, code};

use super::super::super::dto_crud::{PartListQuery, PlaceOnShelfRequest, RecallToPendingRequest};
use super::super::PartService;

use super::{
    assert_shelf_maps_process, ensure_transition, mark_batch_with_status_and_meta,
    require_process_chain, validate_batch_ownership, validate_shelf_zone, BatchListRow,
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
        let step_id =
            ProcessChainRepo::resolve_step_id_by_process(repo.conn_mut(), chain_id, req.next_process_id)
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
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            Some(step_id),
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // rollup
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        // 事件日志
        repo.insert_part_event(
            NewPartEvent {
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
            },
        )
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
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "PENDING",
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
        repo.insert_part_event(
            NewPartEvent {
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
            },
        )
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "recall 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

    /// `GET /parts/pending-programming`：status=PROGRAMMING 一览（复用 PartListOut）。
    pub async fn list_pending_programming<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
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
        // 强制 status=PROGRAMMING
        let f = PartListFilters {
            customer_ids: &[],
            status: Some("PROGRAMMING"),
            statuses: &[],
            is_urgent: query.is_urgent,
            keyword: Some(query.keyword.as_deref().unwrap_or("")),
            // 2026-09-17 PR-4 守卫修复：list_pending_programming 不透传
            // locations/holder_ids（业务语义固定 PROGRAMMING 状态）
            locations: &[],
            holder_ids: &[],
            sort_by: match query.sort_by.as_deref().unwrap_or("PLANNED_DELIVERY_DATE") {
                "CREATED_AT" => "created_at",
                "UPDATED_AT" => "updated_at",
                "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
                "REQUEST_DATE" => "request_date",
                "SERIAL_NO" => "serial_no",
                "DRAWING_NO" => "drawing_no",
                "NAME" => "name",
                _ => "planned_delivery_date",
            },
            sort_dir: query.sort_dir.as_deref().unwrap_or("ASC"),
            limit,
            offset,
            include_deleted: false,
        };
        let items = repo.list_with_filters(&f).await?;
        let total = repo.count_with_filters(&f).await?;
        // 直接转 PartListItem
        let list_items: Vec<PartListItem> = items
            .into_iter()
            .map(|p| PartListItem {
                part: p,
                customer_name: None,
                l1_customer_name: None,
                location: None,
                holder_name: None,
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
            "SELECT b.id AS id, b.batch_no, b.quantity, b.status, b.location,              b.current_holder_id, COALESCE(s.name, w.name, oc.name) AS holder_name,              s2.process_id AS next_process_id, b.current_process_step_id,              b.delivery_note_id, b.parent_batch_id,              b.version              FROM t_part_batch b              LEFT JOIN t_shelf s ON s.id = b.current_holder_id              LEFT JOIN t_worker w ON w.id = b.current_holder_id              LEFT JOIN t_outsource_company oc ON oc.id = b.current_holder_id              LEFT JOIN t_process_chain_step s2 ON s2.id = b.current_process_step_id              WHERE b.part_id = $1 AND b.deleted_at IS NULL              ORDER BY b.batch_no ASC",
        )
        .bind(part_id)
        .fetch_all(repo.conn_mut())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PartBatchListItemOut {
                id: r.id,
                batch_no: r.batch_no,
                quantity: r.quantity,
                status: r.status,
                location: r.location,
                current_holder_id: r.current_holder_id,
                holder_name: r.holder_name,
                // 2026-09-16 PR-3 批次 step 化：next_process_id 由 step.process_id 派生；
                // DTO 保留字段（兼容前端），但 PartBatchListItemOut 当前**总是 None**
                // —— 见 dto_crud.rs 字段说明。如需该信息请前端改为读
                // current_process_step_id 后端按需派生。
                next_process_id: r.next_process_id,
                delivery_note_id: r.delivery_note_id,
                parent_batch_id: r.parent_batch_id,
                version: r.version,
            })
            .collect())
    }
}