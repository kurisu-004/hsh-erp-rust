//! prod::batch 的上架 / 召回
//!
//! - `POST /api/v2/prod/batches/{batch_id}/place-on-shelf` —— `PENDING` →
//!   `IN_PROCESS` + `location='PRODUCTION_SHELF'`
//! - `POST /api/v2/prod/batches/{batch_id}/recall-to-pending` —— 2026-10-06 订正：
//!   `IN_PROCESS`（`location` 限 `PRODUCTION_SHELF` 或 `WORKER`）/ `PROGRAMMING`
//!   → `PENDING`（召回：把批次退回待下发池，位置 / 持有人 / 工序归属 / step 一并清空）

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::dto::{PlaceOnShelfRequest, RecallToPendingRequest};
use crate::shared::error::{AppError, code};

use super::BatchService;
use crate::shared::batch::guards::{
    assert_shelf_maps_process, ensure_transition, mark_batch_with_status_and_meta,
    optional_process_chain, optional_step_id, validate_batch_version, validate_shelf_zone,
};

impl BatchService {
    /// `POST /prod/batches/{batch_id}/place-on-shelf`：PENDING → IN_PROCESS（PRODUCTION_SHELF）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：
    /// - `req.next_process_id` 经 `optional_step_id` 解析为 step_id 写入
    ///   `t_part_batch.current_process_step_id`（2026-10-03 起链可选，无链落 NULL）
    /// - `current_process_id` 写目标工序（池归属权威依据）
    pub async fn place_on_shelf<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: PlaceOnShelfRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
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
        ensure_transition(from, PartStatus::IN_PROCESS, "place-on-shelf")?;
        // 2026-10-03：工序链可选（无链的旧零件也能上架，见 guard.rs）
        let chain_id = optional_process_chain(repo.conn_mut(), part_id).await?;
        // shelf 校验
        validate_shelf_zone(repo.conn_mut(), req.shelf_id, "PRODUCTION").await?;
        // shelf ↔ process 映射
        assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, req.next_process_id).await?;
        // PR-3：解析 step_id（无链 → NULL；有链但链内无该工序 → 20702）
        let step_id = optional_step_id(repo.conn_mut(), chain_id, req.next_process_id).await?;
        // 翻状态
        // 2026-09-30：进池 → current_process_id 写目标工序（池归属权威依据）
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            // 2026-10-03：无链时为 None ⇒ shared::batch::status 的 clear 分支写 NULL
            step_id,
            Some(req.next_process_id),
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // 2026-10-01 review 第 1 轮 m4：原此处有一行
        // `let _ = PartService::sync_from_batch_change(...)`。shared::batch::status 收口后它是
        // 无害空跑（派生已在 `mark_batch_with_status_and_meta` 内做完），且会让
        // 下一个读代码的人以为「调它」是必须的 —— 已删。
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

    /// `POST /prod/batches/{batch_id}/recall-to-pending`：召回待下发。
    ///
    /// 源状态：`IN_PROCESS`（位置限 `PRODUCTION_SHELF` 或 `WORKER`，见下方守卫）
    /// 或 `PROGRAMMING` → `PENDING`。副作用是把 `location` / `current_holder_id` /
    /// `current_process_id` / `current_process_step_id` 四列一起清 NULL。
    pub async fn recall_to_pending<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: RecallToPendingRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
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
        ensure_transition(from, PartStatus::PENDING, "recall-to-pending")?;
        // 额外 service 守：IN_PROCESS 时批次必须停在「生产中」的位置 —— 在生产架上
        // （PRODUCTION_SHELF）或在工人手上（WORKER）。
        //
        // 2026-10-06 放宽到 WORKER：运营需要把**已被工人领走**的批次一键召回，
        // 否则只能等工人手工报工 / 归还。放宽是安全的，因为守卫之后的
        // `mark_batch_with_status_and_meta(..., "PENDING", None, None, None, None, ...)`
        // 里 4 个 `None` 触发 shared::batch::status 的 `clear_location` / `clear_holder_id` /
        // `clear_process_id` / `clear_process_step_id`（三态约定：`None` = 保持原值，
        // 「清 NULL」由同名 `clear_*` 显式表达）⇒ 一次写入把 location、holder、
        // 工序归属、step 四列一起清空：
        // - `current_process_id` 清 NULL ⇒ 批次不再命中任何工序候选池（候选池 SQL 硬限定
        //   `status='IN_PROCESS' AND location='PRODUCTION_SHELF' AND current_process_id = ANY(...)`）
        // - `location` + `current_holder_id` 清 NULL ⇒ 不残留在工人持有列表（该列表硬限定
        //   `location='WORKER' AND current_holder_id = $worker_id`），工位容量是按这两列
        //   实时 COUNT 出来的，无需额外回收动作
        // 其余 location（INSPECTION_SHELF / OUTSOURCE_COMPANY / OFFICE / NULL）仍拒绝。
        if from == PartStatus::IN_PROCESS
            && !matches!(
                batch.location.as_deref(),
                Some("PRODUCTION_SHELF" | "WORKER")
            )
        {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "recall-to-pending: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上或被工人持有\
                     （当前 location={}）",
                    batch.location.as_deref().unwrap_or("NULL")
                ),
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
}
