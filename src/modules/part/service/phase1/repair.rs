//! Phase 1 / 1.4 返修闭环
//!
//! 方法：`complete_repair` / `repair_dispatch` / `list_repair_batches` /
// `list_repairing_batches` + 共享 `list_batches_with_status` helper。
//!
//! 2026-09-22 D-6：从原 `phase1.rs` 按业务动作拆出。共享 helper 在 `phase1/mod.rs` 同 crate 内可见。



use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::dto::{
    InspectionBatchListItemOut, InspectionBatchListOut, InspectionBatchListQuery,
};
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

use super::super::super::dto_crud::{CompleteRepairRequest, RepairDispatchRequest};
use super::super::PartService;

use super::{
    assert_shelf_maps_process, mark_batch_with_status_and_meta, require_process_chain,
    validate_batch_ownership, InspectionRepairRow,
};

impl PartService {
    // ===== 1.4 返修闭环 =====

    /// `POST /parts/{id}/complete-repair`：REPAIRING → IN_PROCESS（落回生产架）
    /// 或 REPAIRING → INSPECTION（送检区）。
    pub async fn complete_repair<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: CompleteRepairRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
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
        if from != PartStatus::REPAIRING {
            return Err(AppError::biz(
                code::BIZ_PART_REPAIR_NOT_TRIGGERED,
                "complete-repair: 源状态必须是 REPAIRING",
            ));
        }
        // shelf 区决定目标状态
        let shelf = ShelfRepo::get_by_id(repo.conn_mut(), req.shelf_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_SHELF_NOT_FOUND, "shelf 不存在"))?;
        if !shelf.is_active {
            return Err(AppError::biz(code::BIZ_SHELF_INACTIVE, "shelf 已停用"));
        }
        let (new_status, new_location, step_id_opt) = match shelf.zone.as_str() {
            "PRODUCTION" => {
                let np = req.next_process_id.ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "PRODUCTION 区需要 next_process_id")
                })?;
                // PR-3：PRODUCTION 区必须已绑定工艺链
                let chain_id = require_process_chain(repo.conn_mut(), part_id).await?;
                assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, np).await?;
                // PR-3：解析 step_id
                let step_id =
                    ProcessChainRepo::resolve_step_id_by_process(repo.conn_mut(), chain_id, np)
                        .await?
                        .ok_or_else(|| {
                            AppError::biz(
                                code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                                format!(
                                    "chain {} 内找不到 process_id={} 的活跃 step",
                                    chain_id, np
                                ),
                            )
                        })?;
                ("IN_PROCESS", Some("PRODUCTION_SHELF"), Some(step_id))
            }
            "INSPECTION" => {
                // INSPECTION 区不带 step（送检区不需要 process 上下文）；
                // 检验完成后 to_process / to_ship 再设 step
                ("INSPECTION", Some("INSPECTION_SHELF"), None)
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("complete-repair 不允许 zone={other}"),
                ));
            }
        };
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            new_status,
            new_location,
            Some(req.shelf_id),
            step_id_opt,
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
                event_type: "REPAIR_COMPLETED",
                from_status: Some("REPAIRING"),
                to_status: Some(new_status),
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
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "complete-repair 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/repair-dispatch`：一步式返修下发。
    ///
    /// 入口：IN_PROCESS / INSPECTION / READY_TO_SHIP；目标状态由 shelf.zone 决定
    /// （PRODUCTION → IN_PROCESS；INSPECTION → INSPECTION）。
    pub async fn repair_dispatch<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: RepairDispatchRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
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
        // 入口白名单（PR-M 2026-08-04：INSPECTION / READY_TO_SHIP / IN_PROCESS / DELIVERED）
        if !matches!(
            from,
            PartStatus::INSPECTION
                | PartStatus::READY_TO_SHIP
                | PartStatus::IN_PROCESS
                | PartStatus::DELIVERED
        ) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("repair-dispatch: 起点 {from:?} 不允许"),
            ));
        }
        let shelf = ShelfRepo::get_by_id(repo.conn_mut(), req.shelf_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_SHELF_NOT_FOUND, "shelf 不存在"))?;
        if !shelf.is_active {
            return Err(AppError::biz(code::BIZ_SHELF_INACTIVE, "shelf 已停用"));
        }
        let (new_status, new_location, step_id_opt) = match shelf.zone.as_str() {
            "PRODUCTION" => {
                let np = req.next_process_id.ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "PRODUCTION 区需要 next_process_id")
                })?;
                // PR-3：PRODUCTION 区必须已绑定工艺链
                let chain_id = require_process_chain(repo.conn_mut(), part_id).await?;
                assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, np).await?;
                // PR-3：解析 step_id
                let step_id =
                    ProcessChainRepo::resolve_step_id_by_process(repo.conn_mut(), chain_id, np)
                        .await?
                        .ok_or_else(|| {
                            AppError::biz(
                                code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                                format!(
                                    "chain {} 内找不到 process_id={} 的活跃 step",
                                    chain_id, np
                                ),
                            )
                        })?;
                ("IN_PROCESS", Some("PRODUCTION_SHELF"), Some(step_id))
            }
            "INSPECTION" => ("INSPECTION", Some("INSPECTION_SHELF"), None),
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("repair-dispatch 不允许 zone={other}"),
                ));
            }
        };
        // 一步式：先发 REPAIR_STARTED 事件，再 UPDATE；状态机视角：起 → REPAIRING → 目标
        // 但本实现是「一步式」：单条 UPDATE + 两条事件日志（REPAIR_STARTED + REPAIR_COMPLETED），
        // 与 Python `repair_dispatch` 一致。
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            new_status,
            new_location,
            Some(req.shelf_id),
            step_id_opt,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // 2026-09-16 PR-2 瘦身（migration 027）：t_part_batch 删
        // `has_been_repaired` 列；返修事实由下方两条 t_part_event 事件日志
        // 追溯（REPAIR_STARTED + REPAIR_COMPLETED）。
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        // 两条事件
        repo.insert_part_event(
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "REPAIR_STARTED",
                from_status: Some(from.as_str()),
                to_status: Some("REPAIRING"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.reason.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        repo.insert_part_event(
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "REPAIR_COMPLETED",
                from_status: Some("REPAIRING"),
                to_status: Some(new_status),
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
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "repair-dispatch 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `GET /parts/repair-batches`：DELIVERED 批次列表（M+C+I）。
    /// 复用 `InspectionBatchListQuery` + repo；status=DELIVERED。
    pub async fn list_repair_batches<R: PartRepoTrait>(
        mut repo: R,
        query: &InspectionBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        Self::list_batches_with_status(repo.conn_mut(), query, &["DELIVERED"], current).await
    }

    /// `GET /parts/repairing-batches`：REPAIRING 批次列表（M+C+I）。
    pub async fn list_repairing_batches<R: PartRepoTrait>(
        mut repo: R,
        query: &InspectionBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        Self::list_batches_with_status(repo.conn_mut(), query, &["REPAIRING"], current).await
    }

    /// 通用 INSPECTION / DELIVERED / REPAIRING / 等批次列表实现。
    async fn list_batches_with_status(
        conn: &mut PgConnection,
        query: &InspectionBatchListQuery,
        statuses: &[&str],
        _current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        let limit = query.limit.unwrap_or(200).clamp(1, 500);
        let offset = query.offset.unwrap_or(0).max(0);
        let keyword = query.keyword.as_deref().unwrap_or("");
        let rows: Vec<InspectionRepairRow> = sqlx::query_as::<_, InspectionRepairRow>(
            "SELECT b.id AS batch_id, b.part_id, b.batch_no, b.quantity, b.status,              b.location, b.version, b.current_process_step_id, b.parent_batch_id,              b.current_holder_id, COALESCE(s.name, w.name, oc.name) AS holder_name,              s2.process_id AS next_process_id, p2.name AS next_process_name,              b.delivery_note_id, dn.delivery_note_no,              p.serial_no, p.drawing_no, p.name, p.order_no, p.planned_delivery_date,              p.is_urgent, p.version AS part_version, p.created_at, p.updated_at,              p.customer_id, c.name AS customer_name, c_l1.name AS l1_customer_name              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              LEFT JOIN t_customer c ON c.id = p.customer_id              LEFT JOIN t_customer c_l1 ON c_l1.id = c.parent_id AND c_l1.deleted_at IS NULL              LEFT JOIN t_shelf s ON s.id = b.current_holder_id              LEFT JOIN t_worker w ON w.id = b.current_holder_id              LEFT JOIN t_outsource_company oc ON oc.id = b.current_holder_id              LEFT JOIN t_process_chain_step s2 ON s2.id = b.current_process_step_id              LEFT JOIN t_process p2 ON p2.id = s2.process_id              LEFT JOIN t_delivery_note dn ON dn.id = b.delivery_note_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND b.status = ANY($1)              AND ($2 = '' OR p.drawing_no ILIKE '%' || $2 || '%' OR p.name ILIKE '%' || $2 || '%')              AND ($3::bigint IS NULL OR p.customer_id = $3)              AND ($4::text IS NULL OR p.serial_no ILIKE '%' || $4 || '%')              AND ($5::date IS NULL OR p.planned_delivery_date >= $5)              AND ($6::date IS NULL OR p.planned_delivery_date <= $6)              ORDER BY b.id DESC LIMIT $7 OFFSET $8",
        )
        .bind(statuses)
        .bind(keyword)
        .bind(query.customer_id)
        .bind(query.serial_no.as_deref())
        .bind(query.planned_delivery_date_from)
        .bind(query.planned_delivery_date_to)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *conn)
        .await?;
        let items: Vec<InspectionBatchListItemOut> = rows
            .into_iter()
            .map(|r| InspectionBatchListItemOut {
                batch_id: r.batch_id,
                batch_no: r.batch_no,
                quantity: r.quantity,
                status: r.status,
                location: r.location,
                version: r.version,
                current_process_step_id: r.current_process_step_id,
                parent_batch_id: r.parent_batch_id,
                current_holder_id: r.current_holder_id,
                holder_name: r.holder_name,
                next_process_id: r.next_process_id,
                next_process_name: r.next_process_name,
                delivery_note_id: r.delivery_note_id,
                delivery_note_no: r.delivery_note_no,
                part_id: r.part_id,
                serial_no: r.serial_no,
                drawing_no: r.drawing_no,
                name: r.name,
                order_no: r.order_no,
                planned_delivery_date: r.planned_delivery_date,
                is_urgent: r.is_urgent,
                part_version: r.part_version,
                created_at: r.created_at,
                updated_at: r.updated_at,
                customer_id: r.customer_id,
                customer_name: r.customer_name,
                l1_customer_name: r.l1_customer_name,
            })
            .collect();
        // 计数（轻量重发一次同条件但不带分页）
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) AS n \
             FROM t_part_batch b JOIN t_part p ON p.id = b.part_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
             AND b.status = ANY($1) \
             AND ($2 = '' OR p.drawing_no ILIKE '%' || $2 || '%' OR p.name ILIKE '%' || $2 || '%') \
             AND ($3::bigint IS NULL OR p.customer_id = $3) \
             AND ($4::text IS NULL OR p.serial_no ILIKE '%' || $4 || '%') \
             AND ($5::date IS NULL OR p.planned_delivery_date >= $5) \
             AND ($6::date IS NULL OR p.planned_delivery_date <= $6)",
        )
        .bind(statuses)
        .bind(keyword)
        .bind(query.customer_id)
        .bind(query.serial_no.as_deref())
        .bind(query.planned_delivery_date_from)
        .bind(query.planned_delivery_date_to)
        .fetch_one(&mut *conn)
        .await?;
        Ok(InspectionBatchListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}