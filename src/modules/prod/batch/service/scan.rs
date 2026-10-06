//! prod::batch 的两条扫码快捷入口
//!
//! - `POST /api/v2/prod/batches/{batch_id}/scan-inspect` —— 一步式
//!   `{PENDING, PROGRAMMING, IN_PROCESS}` → INSPECTION → READY_TO_SHIP（pass=true）
//!   或 `IN_PROCESS + is_repairing=true`（pass=false，批次停在送检架等
//!   `complete-repair` 落回生产架）
//! - `POST /api/v2/prod/batches/scan/deliver` —— 司机扫码发货，`serial_no` 反查批次

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::dto::{ScanDeliverPartRequest, ScanInspectRequest};
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::shared::error::{AppError, code};

use super::BatchService;
use crate::shared::batch::guards::{
    mark_batch_status_only, mark_batch_with_status_and_meta, validate_batch_version,
    validate_shelf_zone,
};

impl BatchService {
    // ===== 1.7 扫码检 / 司机扫码 =====

    /// `POST /prod/batches/{batch_id}/scan-inspect`：扫码快捷品检（一步式）。
    ///
    /// `{PENDING, PROGRAMMING, IN_PROCESS}` → INSPECTION（target_shelf）→
    /// READY_TO_SHIP（pass=true）或 IN_PROCESS + `is_repairing = true`
    /// （pass=false，批次停在送检架等 `complete-repair` 落回生产架；shelf_id +
    /// next_process_id 由 DTO 承载，作用于后续那次 complete-repair）。
    pub async fn scan_inspect<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: ScanInspectRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Inspector])?;
        let batch = repo
            .find_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查。
        let part_id = batch.part_id;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        validate_batch_version(batch.id, req.version, batch.version)?;
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
        validate_shelf_zone(
            repo.conn_mut(),
            req.target_inspection_shelf_id,
            "INSPECTION",
        )
        .await?;
        // 第一步：到 INSPECTION
        // 2026-09-30：出池（转 INSPECTION）→ current_process_id 置 NULL，
        // 否则 INSPECTION 批次会混进工序候选池
        let n1 = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "INSPECTION",
            Some("INSPECTION_SHELF"),
            Some(req.target_inspection_shelf_id),
            None,
            None,
            current.id,
        )
        .await?;
        if n1 == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let mid_version = batch.version + 1;
        // 第二步：pass=true → READY_TO_SHIP（清返修标记）；pass=false → 起返修
        //
        // 2026-10-01（REPAIRING 降级为 `t_part_batch.is_repairing` 标记列，
        // migration 005/006）：两个分支都**显式**写标记。
        // - FAIL 分支写 `Some(true)`：这正是「开始返修」的唯一写点（另一处是
        //   `start_repair` 端点的 `mark_batch_repairing`），status 保持
        //   IN_PROCESS —— 返修仍在生产中，progress 与原 REPAIRING 同档 2，
        //   rollup 派生结果与改造前逐字相同。
        // - pass 分支写 `Some(false)`：品检**通过**意味着这批货不再是「返修中」
        //   （哪怕它上一轮确实返修过 —— 「曾经返修」的历史事实由
        //   `t_part_event` 的 INSPECTION_FAILED 事件追溯，不需要靠标记位留住）。
        //   ⚠️ 事实上第一步（转 INSPECTION）已经经 `mark_batch_with_status_and_meta`
        //   把标记清成 false 了，这里再显式写一次是**冗余但显式**：让每个分支的
        //   不变式在本行自证，不依赖「上一步恰好清了」这种跨函数推理。
        //
        // 2026-10-01 review 第 1 轮 m6：包装函数恒返回 1（0 行已由 shared::batch::status
        // 转成 `VERSION_CONFLICT` 抛出），原 `if n2 == 0` 是死代码，已删。
        // 两个目标状态都不是终态 → `event_id` 传 `None`。
        if req.pass {
            mark_batch_status_only(
                repo.conn_mut(),
                batch.id,
                mid_version,
                "READY_TO_SHIP",
                Some(false),
                current.id,
                None,
            )
            .await?;
        } else {
            // FAIL：INSPECTION → IN_PROCESS + is_repairing=true；location 仍是
            // INSPECTION_SHELF（返修期间的物理位置由 complete_repair 接管，它把
            // 批次落到生产架或送检架并清标记）。
            mark_batch_status_only(
                repo.conn_mut(),
                batch.id,
                mid_version,
                "IN_PROCESS",
                Some(true),
                current.id,
                None,
            )
            .await?;
        }
        // 事件日志（两条：INSPECTED + INSPECTION_RESULT）
        repo.insert_part_event(NewPartEvent {
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
        })
        .await?;
        // 2026-10-01：`to_status` 写**真实**状态，不再写 'REPAIRING'。
        // `t_part_event.to_status` 是时间线上展示的「这一步走到了哪个状态」，
        // 而 REPAIRING 已不是任何一列会取到的值 —— 写它会让同一条批次在
        // `GET /parts/{id}/events` 里显示的状态与 `GET /parts/{id}` 的
        // `status` 互相矛盾。返修语义由 `event_type='INSPECTION_FAILED'` +
        // `t_part_batch.is_repairing` 承载。
        let (to_status, event_type) = if req.pass {
            ("READY_TO_SHIP", "BATCH_PASSED")
        } else {
            ("IN_PROCESS", "INSPECTION_FAILED")
        };
        repo.insert_part_event(NewPartEvent {
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
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "scan-inspect 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

    /// `POST /prod/batches/scan/deliver`：司机扫码发货。
    /// `part_serial_no` 反查 part_id；`worker_badge_code` 校验必须是「送货司机」工种。
    /// 状态机：`READY_TO_SHIP` → `DELIVERED`（复用 `deliver` 流程的核心）。
    pub async fn scan_deliver_part<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: ScanDeliverPartRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
        // 反查 part
        let part: Option<crate::modules::part::model::TPart> =
            sqlx::query_as::<_, crate::modules::part::model::TPart>(
                "SELECT id, serial_no, name, drawing_no, applicant_name, quantity, \
             request_date, planned_delivery_date, \
             customer_id, assembly_id, status, \
             is_urgent, next_process_id, \
             order_no, system_delivery_date, note, \
             unit_price, total_price, \
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
        // 事件
        repo.insert_part_event(NewPartEvent {
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
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part.id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "scan_deliver 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }
}
