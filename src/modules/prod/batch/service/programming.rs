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
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::shared::error::{AppError, code};

use super::BatchService;
use super::guard::{
    assert_shelf_maps_process, ensure_transition, mark_batch_with_status_and_meta,
    require_process_chain, validate_batch_version, validate_shelf_zone,
};

impl BatchService {
    /// `POST /prod/batches/{batch_id}/release-from-programming`：PROGRAMMING → IN_PROCESS（PRODUCTION_SHELF）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：chain 必须性守卫 + req.next_process_id
    /// 解析为 step_id 写入 current_process_step_id。
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
        // PR-3：part 必须已绑定工艺链
        let chain_id = require_process_chain(repo.conn_mut(), part_id).await?;
        validate_shelf_zone(repo.conn_mut(), req.shelf_id, "PRODUCTION").await?;
        assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, req.next_process_id).await?;
        // PR-3：解析 step_id
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
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            Some(step_id),
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
