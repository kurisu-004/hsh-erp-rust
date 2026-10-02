//! prod::batch 的手动 pick-up：`POST /api/v2/prod/batches/{batch_id}/pick-up`
//!
//! 起点 `PENDING` / `IN_PROCESS+PRODUCTION_SHELF` → 目标 `IN_PROCESS+WORKER`。
//! Manager / Clerk / ShelfAccount 三角色可触发；worker 必须 active 且绑定 work_type。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::dto::PickUpRequest;
use crate::shared::error::{AppError, code};

use super::BatchService;
use super::guard::{mark_batch_with_status_and_meta, validate_batch_version, validate_shelf_zone};

impl BatchService {
    /// `POST /prod/batches/{batch_id}/pick-up`：手动 pick-up。
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
        batch_id: i64,
        req: PickUpRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::ShelfAccount])?;
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
        // 2026-09-30：同理透传 batch.current_process_id（池归属权威依据；
        // pick-up 不改工序，只换 holder）
        let n = if from == PartStatus::PENDING {
            mark_batch_with_status_and_meta(
                repo.conn_mut(),
                batch.id,
                req.version,
                "IN_PROCESS",
                Some("WORKER"),
                Some(req.worker_id),
                batch.current_process_step_id,
                batch.current_process_id,
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
        repo.insert_part_event(NewPartEvent {
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
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "pick-up 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }
}
