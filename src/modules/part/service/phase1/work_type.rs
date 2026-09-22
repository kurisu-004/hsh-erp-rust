//! Phase 2 (2026-09-13) — 领取链路 (B 方案：手动 pick-up 兜底)
//!
//! 方法：`pick_up` / `list_by_work_type` / `list_pickable_by_work_type` /
// `list_by_worker`。
//!
//! 2026-09-22 D-6：从原 `phase1.rs` 按业务动作拆出。共享 helper 在 `phase1/mod.rs` 同 crate 内可见。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part::vo::{PartListItem, PartListOut};
use crate::shared::error::{AppError, code};

use super::super::super::dto_crud::{ByWorkTypeQuery, ByWorkerQuery, PickUpRequest};
use super::super::PartService;

use super::{
    mark_batch_with_status_and_meta, validate_batch_ownership, validate_shelf_zone,
};

impl PartService {
    // ===== Phase 2 (2026-09-13) — 领取链路 (B 方案：手动 pick-up 兜底) =====

    /// `POST /parts/{id}/pick-up`：手动 pick-up。
    /// 起点：PENDING / IN_PROCESS+PRODUCTION_SHELF → 目标 IN_PROCESS+WORKER。
    ///
    /// 不变量：
    /// - worker 必须 is_active 且 work_type_id 不为 NULL
    /// - shelf 必须 zone=PRODUCTION 且 active
    /// - 状态机迁移：`{PENDING, IN_PROCESS} → IN_PROCESS`（DB 状态相同；service 守 location）
    /// - 写 PICKED_UP 事件 + 广播 PART_PICKED_UP WS
    pub async fn pick_up<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: PickUpRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::ShelfAccount])?;
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
        if from != PartStatus::PENDING && from != PartStatus::IN_PROCESS {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("pick-up 起点 {from:?} 不允许"),
            ));
        }
        if from == PartStatus::IN_PROCESS && batch.location.as_deref() != Some("PRODUCTION_SHELF") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "pick-up: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上",
            ));
        }
        // worker 必须 active + 有 work_type
        let worker: Option<(bool, Option<i64>)> = sqlx::query_as(
            "SELECT is_active, work_type_id FROM t_worker \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(req.worker_id)
        .fetch_optional(repo.conn_mut())
        .await?;
        let (is_active, wt_id) = worker.ok_or_else(|| {
            AppError::biz(
                code::BIZ_WORKER_NOT_FOUND,
                format!("worker {} 不存在", req.worker_id),
            )
        })?;
        if !is_active {
            return Err(AppError::biz(
                code::BIZ_WORKER_INACTIVE,
                format!("worker {} 已停用", req.worker_id),
            ));
        }
        if wt_id.is_none() {
            return Err(AppError::biz(
                code::BIZ_WORKER_NO_WORK_TYPE,
                format!("worker {} 未绑定 work_type", req.worker_id),
            ));
        }
        validate_shelf_zone(repo.conn_mut(), req.shelf_id, "PRODUCTION").await?;
        // 翻状态：PENDING → IN_PROCESS+WORKER；IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER
        // PR-3：保留 batch.current_process_step_id（pick-up 不改 step，只换 holder）
        let n = if from == PartStatus::PENDING {
            mark_batch_with_status_and_meta(
                repo.conn_mut(),
                batch.id,
                req.version,
                "IN_PROCESS",
                Some("WORKER"),
                Some(req.worker_id),
                batch.current_process_step_id,
                current.id,
            )
            .await?
        } else {
            // IN_PROCESS：只翻 location+holder，status 保持 IN_PROCESS
            // PR-3：删 placed_at 写入（列已删）
            sqlx::query(
                "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = $3, \
                 version = version + 1, updated_at = now(), updated_by = $4 \
                 WHERE id = $1 AND version = $2 AND status = 'IN_PROCESS' \
                   AND deleted_at IS NULL",
            )
            .bind(batch.id)
            .bind(req.version)
            .bind(req.worker_id)
            .bind(current.id)
            .execute(repo.conn_mut())
            .await?
            .rows_affected()
        };
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        repo.insert_part_event(
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "PICKED_UP",
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
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "pick-up 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

    /// `GET /parts/by-work-type/{work_type_id}`：可领件（按工种过滤）。
    ///
    /// 实现：worker.work_type_id = $1 → t_part_batch.current_holder_id = worker.id，
    /// 且 batch.location='WORKER'。简化：直接按 worker 反查（每工种有多个 worker）。
    pub async fn list_by_work_type<R: PartRepoTrait>(
        mut repo: R,
        work_type_id: i64,
        query: &ByWorkTypeQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        // 直接列出该工种所有 worker 当前持有的件（IN_PROCESS + location=WORKER）
        let rows: Vec<(i64, String, String, i32, i64, Option<String>)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, b.id AS bid, w.name AS worker_name \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_worker w ON w.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND w.deleted_at IS NULL AND w.is_active = true \
               AND w.work_type_id = $1 \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
             ORDER BY b.id DESC LIMIT $2 OFFSET $3",
        )
        .bind(work_type_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(|(id, serial, drawing, qty, bid, worker_name)| {
                let p = crate::modules::part::model::TPart {
                    id,
                    serial_no: Some(serial),
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    is_urgent: false,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    process_chain_id: None,
                };
                let item: PartListItem = PartListItem {
                    part: p,
                    customer_name: None,
                    l1_customer_name: None,
                    location: None,
                    holder_name: None,
                };
                // 附加 worker_name（轻量：DTO 上没字段，仅放 batch_id 展示）
                let _ = bid;
                let _ = worker_name;
                item
            })
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_worker w ON w.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND w.deleted_at IS NULL \
               AND w.is_active = true AND w.work_type_id = $1 \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER'",
        )
        .bind(work_type_id)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/pickable-by-work-type/{work_type_id}`：可领取件（货架上、绑了对应工序）。
    pub async fn list_pickable_by_work_type<R: PartRepoTrait>(
        mut repo: R,
        work_type_id: i64,
        query: &ByWorkTypeQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let shelf_filter = query.shelf_id;
        // 列：t_part_batch WHERE location=PRODUCTION_SHELF AND batch.next_process_id IN (工种→工序映射)
        let rows: Vec<(i64, String, String, i32, Option<i64>)> = sqlx::query_as(
            // PR-3：next_process_id 改读 step.process_id（JOIN t_process_chain_step）
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, s.process_id \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_process_chain_step s ON s.id = b.current_process_step_id \
             JOIN t_work_type_process wtp ON wtp.process_id = s.process_id \
             JOIN t_shelf sh ON sh.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2) \
             ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, b.id ASC \
             LIMIT $3 OFFSET $4",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(
                |(id, serial, drawing, qty, _np)| PartListItem {
                    part: crate::modules::part::model::TPart {
                        id,
                        serial_no: Some(serial),
                        name: drawing.clone(),
                        drawing_no: drawing,
                        applicant_name: String::new(),
                        quantity: qty,
                        request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                        planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                        customer_id: 0,
                        assembly_id: None,
                        status: "IN_PROCESS".to_string(),
                        is_urgent: false,
                        next_process_id: None,
                        order_no: None,
                        system_delivery_date: None,
                        note: None,
                        version: 0,
                        created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                        created_by: None,
                        updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                        updated_by: None,
                        deleted_at: None,
                        process_chain_id: None,
                    },
                    customer_name: None,
                    l1_customer_name: None,
                    location: None,
                    holder_name: None,
                },
            )
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_process_chain_step s ON s.id = b.current_process_step_id \
             JOIN t_work_type_process wtp ON wtp.process_id = s.process_id \
             JOIN t_shelf sh ON sh.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2)",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/by-worker/{worker_id}`：工人当前持有件。
    pub async fn list_by_worker<R: PartRepoTrait>(
        mut repo: R,
        worker_id: i64,
        query: &ByWorkerQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let rows: Vec<(i64, String, String, i32)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1 \
             ORDER BY b.id DESC LIMIT $2 OFFSET $3",
        )
        .bind(worker_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(
                |(id, serial, drawing, qty)| PartListItem {
                    part: crate::modules::part::model::TPart {
                        id,
                        serial_no: Some(serial),
                        name: drawing.clone(),
                        drawing_no: drawing,
                        applicant_name: String::new(),
                        quantity: qty,
                        request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                        planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                        customer_id: 0,
                        assembly_id: None,
                        status: "IN_PROCESS".to_string(),
                        is_urgent: false,
                        next_process_id: None,
                        order_no: None,
                        system_delivery_date: None,
                        note: None,
                        version: 0,
                        created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                        created_by: None,
                        updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                        updated_by: None,
                        deleted_at: None,
                        process_chain_id: None,
                    },
                    customer_name: None,
                    l1_customer_name: None,
                    location: None,
                    holder_name: None,
                },
            )
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1",
        )
        .bind(worker_id)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}