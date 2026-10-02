//! prod::batch 的批次终态与返修起点
//!
//! - `POST /api/v2/prod/batches/{batch_id}/deliver` —— 出货（`READY_TO_SHIP` →
//!   `DELIVERED`），OCC 锚 `t_part_batch.version`
//! - `POST /api/v2/prod/batches/{batch_id}/complete` —— 完工（`DELIVERED` →
//!   `COMPLETED`）；`task::auto_complete` 也调它做定时自动完工
//! - `POST /api/v2/prod/batches/{batch_id}/start-repair` —— 起修（只置
//!   `is_repairing = true`，不翻 `status`）
//!
//! part 域的 `cancel` / `force-complete` 留在 `part::service::lifecycle` —— 它们的
//! 操作对象是 part（一个 part 的全部活跃批次），不是单个批次。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part::vo::PartOut;
use crate::modules::prod::batch::dto::{CompleteRequest, DeliverRequest, StartRepairRequest};
use crate::shared::error::{AppError, code};

use super::BatchService;

impl BatchService {
    /// deliver (PR-B3 batch 级)：锚定 `t_part_batch.version`，状态机守卫读
    /// batch 当前状态 `READY_TO_SHIP → DELIVERED`。
    ///
    /// BREAKING CHANGE：DTO 新增 `batch_id` + `version`（前端从
    /// `GET /parts/by-serial/{serial_no}/part-batches` 取 batch.id + version）。
    pub async fn deliver<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: DeliverRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查；原先的 `batch.part_id != part_id` 断言恒真，已删。
        let batch = repo.find_batch_by_id(batch_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {batch_id} 不存在"),
            )
        })?;
        let part_id = batch.part_id;
        // 1. 读 part（仅 need drawing_no 用于事件日志 + 终态守卫；其它派生列
        //    由 rollup 在 batch 翻转后回填）。
        let part = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
        })?;
        if PartStatus::from_str(&part.status) == Some(PartStatus::CANCELLED) {
            return Err(AppError::biz(
                code::BIZ_PART_ALREADY_CANCELLED,
                "工单已 CANCELLED",
            ));
        }
        // 2. 定位 batch（必须属于 part + READY_TO_SHIP + 未软删）。
        // 3. 状态机守卫：读 batch 当前状态（不是 part 派生列）。
        let from = PartStatus::from_str(&batch.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("batch {} status 非法: {}", batch.id, batch.status),
            )
        })?;
        if !from.can_transition_to(PartStatus::DELIVERED) {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_READY_TO_SHIP,
                format!(
                    "batch {} 当前状态 {} 不允许 deliver（必须 READY_TO_SHIP）",
                    batch.id,
                    from.as_str()
                ),
            ));
        }
        // 4. caller 侧乐观锁：锚定 batch.version。
        if batch.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!(
                    "batch {} 版本冲突（期望 {}，实际 {}）",
                    batch.id, req.version, batch.version
                ),
            ));
        }
        // 5. UPDATE batch: READY_TO_SHIP → DELIVERED（OCC）。
        let bn = repo
            .mark_batch_delivered(batch.id, batch.version, current.id)
            .await?;
        if bn == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("batch {} 版本冲突", batch.id),
            ));
        }
        // 6. 2026-10-01：`mark_batch_delivered` 走 status_gate，part → assembly
        //    派生已在同一事务内完成（多批次场景下按 min-progress 决定 part 状态），
        //    **不再**手工调 `sync_from_batch_change`（原 `let _ =` 既冗余，又会把
        //    rollup 错误静默丢弃）。
        // 7. 事件日志：batch_id + quantity 来自操作的批次。
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "DELIVERED",
            from_status: Some("READY_TO_SHIP"),
            to_status: Some("DELIVERED"),
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
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "deliver 后查不到"))?;
        Ok(PartOut::from(fresh))
    }

    pub async fn complete<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: CompleteRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查；原先的 `batch.part_id != part_id` 断言恒真，已删。
        let batch = repo.find_batch_by_id(batch_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {batch_id} 不存在"),
            )
        })?;
        let part_id = batch.part_id;
        let part = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
        })?;
        if PartStatus::from_str(&part.status) == Some(PartStatus::CANCELLED) {
            return Err(AppError::biz(
                code::BIZ_PART_ALREADY_CANCELLED,
                "工单已 CANCELLED",
            ));
        }
        // 1. 定位 batch。
        // 2. 状态机守卫：读 batch 当前状态。
        let from = PartStatus::from_str(&batch.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("batch {} status 非法: {}", batch.id, batch.status),
            )
        })?;
        if !from.can_transition_to(PartStatus::COMPLETED) {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_DELIVERED,
                format!(
                    "batch {} 当前状态 {} 无法 complete（必须 DELIVERED）",
                    batch.id,
                    from.as_str()
                ),
            ));
        }
        // 3. caller 侧乐观锁：锚定 batch.version。
        if batch.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!(
                    "batch {} 版本冲突（期望 {}，实际 {}）",
                    batch.id, req.version, batch.version
                ),
            ));
        }
        // 4. UPDATE batch: DELIVERED → COMPLETED（OCC）。
        //    ⚠️ `mark_batch_completed` 经 status_gate 写，0 行已由 status_gate
        //    转成 `VERSION_CONFLICT` 抛出，不再重复判 0。
        //
        //    2026-10-01：**序列号释放已下沉进 status_gate 的 rollup**
        //    （step 4）。原实现在这里调 `clear_part_serial_no_when_completed`，
        //    而它的 WHERE 是 **part 级**的 `status='COMPLETED'`，本方法一次只翻
        //    **一条批次** —— 多批次工单完成其中一条时 part 仍是 DELIVERED，该
        //    UPDATE 命中 0 行，再被 `let _ =` 静默吞掉，序列号被
        //    `uk_t_part_serial_no` **永久**占住（无任何告警）。现在
        //    「part 被 rollup 进终态」即释放，与「是否所有批次都完成」彻底解耦。
        let _bn = repo
            .mark_batch_completed(
                batch.id,
                batch.version,
                current.id,
                Some(snowflake.next_id()),
            )
            .await?;
        // 7. 事件日志：batch_id + quantity 来自操作的批次。
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "COMPLETED",
            from_status: Some("DELIVERED"),
            to_status: Some("COMPLETED"),
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
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "complete 后查不到"))?;
        Ok(PartOut::from(fresh))
    }

    pub async fn start_repair<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: StartRepairRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查；原先的 `batch.part_id != part_id` 断言恒真，已删。
        let batch = repo.find_batch_by_id(batch_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {batch_id} 不存在"),
            )
        })?;
        let part_id = batch.part_id;
        let part = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
        })?;
        if PartStatus::from_str(&part.status) == Some(PartStatus::CANCELLED) {
            return Err(AppError::biz(
                code::BIZ_PART_ALREADY_CANCELLED,
                "工单已 CANCELLED",
            ));
        }
        // 1. 定位 batch。
        // 2. 状态机守卫：读 batch 当前状态。
        let from = PartStatus::from_str(&batch.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("batch {} status 非法: {}", batch.id, batch.status),
            )
        })?;
        // 2026-10-01：REPAIRING 降级为 `t_part_batch.is_repairing` 标记后，
        // start-repair **不再是一次状态迁移**（status 保持 IN_PROCESS），
        // 守卫从「枚举迁移白名单」改为「源状态必须是 IN_PROCESS」+「尚未处于
        // 返修中」。
        //
        // `is_repairing` 这条守卫是本轮补齐的**精确判定**：`is_repairing`
        // 语义是「已进入 / 正在返修」，重复起修要么是前端重复提交，要么是用户
        // 对同一批次连点两次 —— 两种都会白白多写一条 REPAIR_STARTED 事件、让
        // 统计域的「期内返修工单数」（`statistics::count_repair_parts` 按
        // distinct part_id 计，同 part 重复起修不会重复计，但换批次就会）失真，
        // 更重要的是会让「已起修 → complete_repair」的配对关系变得不可推。
        // 错误码沿用 `BIZ_PART_REPAIR_NOT_TRIGGERED`（20118）：该码的语义是
        // 「返修流转的前置条件不满足」，重复起修同属此类，无需新增错误码。
        //
        // ⚠️ `PartStatus::from_str` 的 `"REPAIRING" => IN_PROCESS` 过渡兼容
        // 分支让 migration 006 之前的存量行能通过状态守卫（其标记由 006 洗成
        // `is_repairing = true`），故存量返修批次会落进下面这条「已在返修中」
        // 的拒绝 —— 属预期：它本来就已在返修中，不需要再起一次。
        if from != PartStatus::IN_PROCESS {
            return Err(AppError::biz(
                code::BIZ_PART_REPAIR_NOT_TRIGGERED,
                format!(
                    "batch {} 当前状态 {} 无法 start-repair（必须 IN_PROCESS）",
                    batch.id,
                    from.as_str()
                ),
            ));
        }
        if batch.is_repairing {
            return Err(AppError::biz(
                code::BIZ_PART_REPAIR_NOT_TRIGGERED,
                format!("batch {} 已处于返修中，无需重复 start-repair", batch.id),
            ));
        }
        // 3. caller 侧乐观锁：锚定 batch.version。
        if batch.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!(
                    "batch {} 版本冲突（期望 {}，实际 {}）",
                    batch.id, req.version, batch.version
                ),
            ));
        }
        // 4. UPDATE batch（OCC）：2026-10-01 起**不改 status**（仍 IN_PROCESS），
        //    只置 `is_repairing = true`（见 `mark_batch_repairing` 的 doc）。
        //    2026-09-16 PR-2 瘦身（migration 027）：t_part_batch 删
        //    `has_been_repaired` 列；返修事实由下方 REPAIR_STARTED 事件日志 +
        //    `is_repairing` 标记列共同追溯。
        //    ⚠️ `mark_batch_repairing` 经 status_gate 写，0 行已由 status_gate
        //    转成 `VERSION_CONFLICT` 抛出，故不再重复判 0。
        let _bn = repo
            .mark_batch_repairing(batch.id, batch.version, current.id)
            .await?;
        // 5. 2026-10-01：`mark_batch_repairing` 走 status_gate，part → assembly
        //    派生已在同一事务内完成，**不再**手工调 `sync_from_batch_change`
        //    （原 `let _ =` 既冗余，又把 rollup 错误降级成「静默丢弃」）。
        // 6. 事件日志。
        //
        // 2026-10-01：`to_status` 由 `'REPAIRING'` 改为 `'IN_PROCESS'` —— 本端点
        // 不改 status，只翻 `is_repairing` 标记，而 REPAIRING 已不是任何一列会
        // 取到的值（写它会让时间线与 `t_part_batch.status` 矛盾）。形如
        // `IN_PROCESS → IN_PROCESS` 的事件是**真实**轨迹：状态未变、位置未变、
        // 变的是标记位与「已起修」这一事实。返修语义由 `event_type='REPAIR_
        // STARTED'` + `is_repairing` 列承载。
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "REPAIR_STARTED",
            from_status: Some("IN_PROCESS"),
            to_status: Some("IN_PROCESS"),
            batch_id: Some(batch.id),
            quantity: Some(batch.quantity),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.reason.as_deref().or(req.note.as_deref()),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "start-repair 后查不到"))?;
        Ok(PartOut::from(fresh))
    }
}
