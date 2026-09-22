//! Phase 1 / 1.7 扫码检 / 司机扫码
//!
//! 方法：`scan_inspect` / `scan_deliver_part`。
//!
//! 2026-09-22 D-6：从原 `phase1.rs` 按业务动作拆出。共享 helper 在 `phase1/mod.rs` 同 crate 内可见。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::shared::error::{AppError, code};

use super::super::super::dto_crud::{ScanDeliverPartRequest, ScanInspectRequest};
use super::super::PartService;

use super::{mark_batch_status_only, mark_batch_with_status_and_meta, validate_batch_ownership, validate_shelf_zone};

impl PartService {
    // ===== 1.7 扫码检 / 司机扫码 =====

    /// `POST /parts/{id}/scan-inspect`：扫码快捷品检（一步式）。
    ///
    /// `{PENDING, PROGRAMMING, IN_PROCESS}` → INSPECTION（target_shelf）→ READY_TO_SHIP（pass=true）
    /// 或 → REPAIRING（pass=false + shelf_id + next_process_id）。
    pub async fn scan_inspect<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ScanInspectRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Inspector])?;
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
        // 入口白名单
        if !matches!(
            from,
            PartStatus::PENDING | PartStatus::PROGRAMMING | PartStatus::IN_PROCESS
        ) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("scan-inspect: 起点 {from:?} 不允许"),
            ));
        }
        validate_shelf_zone(repo.conn_mut(), req.target_inspection_shelf_id, "INSPECTION").await?;
        // 第一步：到 INSPECTION
        let n1 = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "INSPECTION",
            Some("INSPECTION_SHELF"),
            Some(req.target_inspection_shelf_id),
            None,
            current.id,
        )
        .await?;
        if n1 == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let mid_version = batch.version + 1;
        // 第二步：pass=true → READY_TO_SHIP；pass=false → REPAIRING
        if req.pass {
            let n2 = mark_batch_status_only(
                repo.conn_mut(),
                batch.id,
                mid_version,
                "READY_TO_SHIP",
                current.id,
            )
            .await?;
            if n2 == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "batch 版本冲突（INSPECTION→READY_TO_SHIP）",
                ));
            }
        } else {
            // FAIL：INSPECTION → REPAIRING；保留 shelf 为 INSPECTION_SHELF（carry 状态由下一步 complete_repair 接管）
            // 2026-09-16 PR-2 瘦身（migration 027）：t_part_batch 删
            // `has_been_repaired` 列；返修事实由下方 INSPECTION_FAILED 事件日志追溯。
            let n2 =
                mark_batch_status_only(repo.conn_mut(), batch.id, mid_version, "REPAIRING", current.id)
                    .await?;
            if n2 == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "batch 版本冲突（INSPECTION→REPAIRING）",
                ));
            }
        }
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        // 事件日志（两条：INSPECTED + INSPECTION_RESULT）
        repo.insert_part_event(
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "INSPECTED",
                from_status: Some(from.as_str()),
                to_status: Some("INSPECTION"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: None,
                created_by: Some(current.id),
            },
        )
        .await?;
        let (to_status, event_type) = if req.pass {
            ("READY_TO_SHIP", "BATCH_PASSED")
        } else {
            ("REPAIRING", "INSPECTION_FAILED")
        };
        repo.insert_part_event(
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type,
                from_status: Some("INSPECTION"),
                to_status: Some(to_status),
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
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "scan-inspect 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/scan/deliver-part`：司机扫码发货。
    /// `part_serial_no` 反查 part_id；`worker_badge_code` 校验必须是「送货司机」工种。
    /// 状态机：`READY_TO_SHIP` → `DELIVERED`（复用 `deliver` 流程的核心）。
    pub async fn scan_deliver_part<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: ScanDeliverPartRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
        // 反查 part
        let part: Option<crate::modules::part::model::TPart> =
            sqlx::query_as::<_, crate::modules::part::model::TPart>(
                "SELECT id, serial_no, name, drawing_no, applicant_name, quantity, \
             request_date, planned_delivery_date, \
             customer_id, assembly_id, status, \
             is_urgent, next_process_id, \
             order_no, system_delivery_date, note, \
             version, created_at, created_by, updated_at, updated_by, \
             deleted_at, process_chain_id \
             FROM t_part WHERE serial_no = $1 AND deleted_at IS NULL",
            )
            .bind(&req.part_serial_no)
            .fetch_optional(repo.conn_mut())
            .await?;
        let part = part.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("serial_no {} 找不到 part", req.part_serial_no),
            )
        })?;
        // 校验 worker 是送货司机
        let worker = WorkerRepo::get_by_badge_code(repo.conn_mut(), &req.worker_badge_code, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_WORKER_NOT_FOUND, "工牌码无效"))?;
        if !worker.is_active {
            return Err(AppError::biz(code::BIZ_WORKER_INACTIVE, "工人已停用"));
        }
        // 校验工种
        let wt_code: Option<String> = if let Some(wt_id) = worker.work_type_id {
            sqlx::query_scalar("SELECT code FROM t_work_type WHERE id = $1 AND deleted_at IS NULL")
                .bind(wt_id)
                .fetch_optional(repo.conn_mut())
                .await?
        } else {
            None
        };
        if wt_code.as_deref() != Some("DRIVER") {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                format!("工牌 {} 的工种不是 DRIVER", req.worker_badge_code),
            ));
        }
        // 校验 part.status == READY_TO_SHIP
        let from = PartStatus::from_str(&part.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "part.status 非法"))?;
        if from != PartStatus::READY_TO_SHIP {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                format!("part {} 当前 {} 不允许 deliver", part.id, from.as_str()),
            ));
        }
        // 找 READY_TO_SHIP 批次
        let batch = repo
            .find_inprocess_batch_for_part(part.id, None)
            .await
            .map_err(|_| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "批次不存在"))?;
        let batch = match batch {
            Some(b) if b.status == "READY_TO_SHIP" => b,
            _ => {
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    "找不到 READY_TO_SHIP 批次",
                ));
            }
        };
        let n = repo
            .mark_batch_delivered(batch.id, batch.version, current.id)
            .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // 2026-09-16 PR-2 瘦身（migration 027）：t_part 删 `actual_delivery_date` 列；
        // 实际交付日期由下方 DELIVERED 事件日志写入 t_part_event，统计口径
        // 按事件派生（见 statistics 域）。
        let _ = Self::sync_from_batch_change(&mut repo, part.id, current).await?;
        // 事件
        repo.insert_part_event(
            NewPartEvent {
                id: snowflake.next_id(),
                part_id: part.id,
                event_type: "DELIVERED",
                from_status: Some("READY_TO_SHIP"),
                to_status: Some("DELIVERED"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: Some(&req.worker_badge_code),
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = repo
            .get_part_inspected(part.id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "scan_deliver 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }
}