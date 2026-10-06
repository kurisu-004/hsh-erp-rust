//! prod::queue 的召回用例（`POST /api/v2/prod/queue/recall`）
//!
//! 2026-10-08 自 `prod::batch::service::shelf::recall_to_pending` 搬入并改造：
//!
//! ## 契约变更（**破坏性**，前端同 PR 跟进）
//! - 路径：`POST /api/v2/prod/batches/{batch_id}/recall-to-pending` →
//!   `POST /api/v2/prod/queue/recall`；
//! - `batch_id` 由 **URL 路径参数改为请求体字段**（对齐本域其余写端点
//!   「所有 ID 走 body」的约定：写端点的 `batch_id` 不是资源寻址的一部分而是
//!   操作对象，路径参数形态让同一条路由无法再扩展第二个操作对象）；
//! - 出参由 `part::vo::PartOut`（工单全量投影）改为本域
//!   [`crate::modules::prod::queue::vo::queue::RecallOut`]（3 字段）。
//!
//! ## WS 广播不变
//! commit 后仍发 `PART_RECALLED`（payload = `{"part_id": "…"}`）—— 大屏
//! （`/ws/dashboard`）消费该事件刷新在制清单，改事件名会让大屏静默失联。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::queue::dto::RecallToPendingRequest;
use crate::modules::prod::queue::vo::queue::RecallOut;
use crate::shared::batch::guards::{
    ensure_transition, mark_batch_with_status_and_meta, validate_batch_version,
};
use crate::shared::error::{AppError, code};

use super::queue::QueueService;

impl QueueService {
    /// `POST /api/v2/prod/queue/recall`：召回待下发。
    ///
    /// 源状态：`IN_PROCESS`（位置限 `PRODUCTION_SHELF` 或 `WORKER`）或
    /// `PROGRAMMING` → `PENDING`。副作用是把 `location` / `current_holder_id` /
    /// `current_process_id` / `current_process_step_id` 四列一起清 NULL
    /// （由 `shared::batch::status` 的三态约定 `clear_*` 表达）。
    pub async fn recall_to_pending<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: RecallToPendingRequest,
        current: &CurrentUser,
    ) -> Result<RecallOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let batch = repo
            .find_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        let part_id = batch.part_id;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        validate_batch_version(batch.id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::PENDING, "recall-to-pending")?;
        // IN_PROCESS 时批次必须停在「生产中」的位置：在生产架上或被工人持有。
        //
        // 2026-10-06 放宽到 WORKER：运营需要把**已被工人领走**的批次一键召回，
        // 否则只能等工人手工报工 / 归还。放宽是安全的，因为守卫之后的
        // `mark_batch_with_status_and_meta(..., "PENDING", None, None, None, None, …)`
        // 里 4 个 `None` 触发写入口的 `clear_location` / `clear_holder_id` /
        // `clear_process_id` / `clear_process_step_id`（三态约定：`None` = 保持原值，
        // 「清 NULL」由同名 `clear_*` 显式表达）⇒ 一次写入把 location、holder、
        // 工序归属、step 四列一起清空：
        // - `current_process_id` 清 NULL ⇒ 批次不再命中任何工序候选池；
        // - `location` + `current_holder_id` 清 NULL ⇒ 不残留在工人持有列表，
        //   工位容量按这两列实时 COUNT，无需额外回收动作。
        //
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
        mark_batch_with_status_and_meta(
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
        // version + 1：写入口的 OCC UPDATE 恒 +1（拿不到新值时不重新查 ——
        // 「读回再报」会在同一次事务里多一条 SELECT，且写入口将来若改成
        // `version = version + N`，读回值才是唯一正确的报数；当前 +1 是
        // `StatusChange` 的不变式，读回与推算是同一个数）。
        let version = req.version + 1;
        Ok(RecallOut {
            batch_id: batch.id.to_string(),
            part_id: part_id.to_string(),
            version,
        })
    }
}
