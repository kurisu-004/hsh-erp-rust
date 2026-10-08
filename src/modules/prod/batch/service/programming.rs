//! prod::batch 的 CNC 编程出口：`POST /api/v2/prod/batches/{batch_id}/release-from-programming`
//!
//! PROGRAMMING 状态的**唯一出口**。进入路径（`send-to-programming` /
//! `recall-to-programming`）已下线：编程员通过工艺链 + CNC step（`t_process.is_cnc`）
//! 直接进入生产流，本状态只留出口供历史数据消化。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::dto::PlaceOnShelfRequest;
use crate::shared::error::{AppError, code};

use super::BatchService;
use crate::shared::batch::guards::{
    ensure_transition, mark_batch_with_status_and_meta, optional_process_chain, optional_step_id,
    validate_batch_version,
};

impl BatchService {
    /// `POST /prod/batches/{batch_id}/release-from-programming`：PROGRAMMING → IN_PROCESS（PRODUCTION_SHELF）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：req.next_process_id 解析为 step_id 写入
    /// current_process_step_id（2026-10-03 起链可选，无链落 NULL）。
    pub async fn release_from_programming<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: PlaceOnShelfRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
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
        ensure_transition(from, PartStatus::IN_PROCESS, "release-from-programming")?;
        if from != PartStatus::PROGRAMMING {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "release-from-programming: 源状态必须是 PROGRAMMING",
            ));
        }
        // 2026-10-03：工序链可选（无链的旧零件也能释放，见 guard.rs）
        let chain_id = optional_process_chain(repo.conn_mut(), part_id).await?;
        // 2026-10-10：目标生产架由服务端按负载自动选（`shelf_id` 入参已删除）。
        // 理由与 `shelf.rs::place_on_shelf` 的同款注释逐字相同（两个端点共用
        // `PlaceOnShelfRequest`，守卫也必须是同一段 —— 否则两处各判一次、漂移时
        // 「从编程放出的批次」与「从待下发放出的批次」会被不同的守卫放行）。
        let shelf_id = crate::shared::shelf::select::pick_least_loaded(
            repo.conn_mut(),
            "PRODUCTION",
            Some(req.next_process_id),
            crate::shared::shelf::select::shelf_scope_for(current),
        )
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_SHELF_PROCESS_NOT_FOUND,
                format!(
                    "process {} 无可用生产货架（无 active 映射，或命中的映射其货架均已软删 / \
                     已停用 / 非 PRODUCTION 区 / 不在当前账号 scope 内）",
                    req.next_process_id
                ),
            )
        })?
        .id;
        // PR-3：解析 step_id（无链 → NULL；有链但链内无该工序 → 20702）
        let step_id = optional_step_id(repo.conn_mut(), chain_id, req.next_process_id).await?;
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(shelf_id),
            // 2026-10-03：无链时为 None ⇒ shared::batch::status 的 clear 分支写 NULL
            step_id,
            // 2026-09-30：进池 → current_process_id 写目标工序
            Some(req.next_process_id),
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "CNC_RELEASED",
            from_status: Some("PROGRAMMING"),
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
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "release 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }
}
