//! Phase 1 / 1.5 批次拆分 / 取消
//!
//! 方法：`split_batch` / `cancel_batch`。
//!
//! 2026-09-22 D-6：从原 `phase1.rs` 按业务动作拆出。共享 helper 在 `phase1/mod.rs` 同 crate 内可见。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::shared::error::{AppError, code};

use super::super::super::dto_crud::{CancelBatchRequest, SplitBatchRequest};
use super::super::PartService;

use super::{mark_batch_status_only, validate_batch_ownership};

impl PartService {
    // ===== 1.5 批次拆分 / 取消 =====

    /// `POST /parts/{id}/batches/split`：拆出部分量为新批次。
    ///
    /// 不变量 `Σ(未删批次.quantity) = t_part.quantity` 由
    /// `PartBatchRepo::split_batch` 强制（同一事务内连发 max+1 / INSERT / UPDATE 三条 SQL，
    /// OCC 守源批次）。`quantity` ∈ [1, source.quantity - 1]（split_batch 内部守）。
    pub async fn split_batch<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: SplitBatchRequest,
        current: &CurrentUser,
    ) -> Result<i64, AppError> {
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
        // 数量校验
        let qty: i32 = req.quantity.try_into().map_err(|_| {
            AppError::biz(
                code::BIZ_PART_BATCH_INVALID_QUANTITY,
                "quantity 超出 i32 范围",
            )
        })?;
        if qty <= 0 {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_INVALID_QUANTITY,
                format!("quantity {qty} 必须 > 0"),
            ));
        }
        if qty >= batch.quantity {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_INVALID_QUANTITY,
                format!(
                    "quantity {qty} 必须 < batch.quantity {}（拆批部分量）",
                    batch.quantity
                ),
            ));
        }
        let new_batch_id = snowflake.next_id();
        let when = crate::infra::clock::now_naive();
        // PR-3 批次 step 化：新批次继承源批次的 current_process_step_id；
        // placed_at 列已删，不再传递。
        let new_id = PartBatchRepo::split_batch(
            repo.conn_mut(),
            new_batch_id,
            batch.id,
            req.version,
            part_id,
            qty,
            &batch.status,
            batch.location.as_deref(),
            batch.current_holder_id,
            batch.current_process_step_id,
            when,
            Some(current.id),
            Some(current.id),
        )
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"),
            other => AppError::from(other),
        })?;
        // SPLIT 事件
        repo.insert_part_event(
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "SPLIT",
                from_status: Some(&batch.status),
                to_status: Some(&batch.status),
                batch_id: Some(new_id),
                quantity: Some(qty),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref().or(Some("manual split")),
                created_by: Some(current.id),
            },
        )
        .await?;
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        Ok(new_id)
    }

    /// `POST /parts/{id}/batches/{batch_id}/cancel`：批次级取消。
    pub async fn cancel_batch<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        batch_id: i64,
        req: CancelBatchRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = repo
            .find_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        // 终态保护
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        if from == PartStatus::COMPLETED || from == PartStatus::CANCELLED {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("batch 当前状态 {} 不允许取消", from.as_str()),
            ));
        }
        let n = mark_batch_status_only(repo.conn_mut(), batch.id, req.version, "CANCELLED", current.id)
            .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        repo.insert_part_event(
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "BATCH_CANCELLED",
                from_status: Some(from.as_str()),
                to_status: Some("CANCELLED"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.reason.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "cancel-batch 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }
}