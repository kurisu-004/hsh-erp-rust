//! part 域 lifecycle 业务逻辑
//!
//! 包含 4 个公开方法（工单状态机终态 / 返修翻转）：
//! - `deliver` —— READY_TO_SHIP → DELIVERED
//! - `complete` —— DELIVERED → COMPLETED
//! - `cancel` —— 5 状态白名单 → CANCELLED
//! - `start_repair` —— 2026-10-01 起**不是状态迁移**：源状态 IN_PROCESS +
//!   `is_repairing = false` 时把标记置 true（REPAIRING 已降级为标记列，
//!   migration 005/006）
//!
//! 同步策略：每个生命周期方法在事务内同时翻转 `t_part` 与最近一条匹配的
//! `t_part_batch`（status 白名单匹配 + id DESC）。无 source-status 批次时仅翻
//! `t_part`（新建工单未拆批场景）。
//!
//! 错误码契约（与 `statemachine.rs` 对齐）：
//! - 20101 `BIZ_PART_NOT_FOUND` —— part 不存在 / 软删
//! - 20104 `BIZ_INVALID_VALUE` —— DB 中 status 字符串不在 enum 白名单
//! - 20103 `BIZ_INVALID_TRANSITION` —— 状态机白名单拒绝（cancel 时 COMPLETED / CANCELLED 等）
//! - 20115 `BIZ_PART_ALREADY_CANCELLED` —— 工单已 CANCELLED
//! - 20116 `BIZ_PART_NOT_DELIVERED` —— complete 要求 DELIVERED
//! - 20117 `BIZ_PART_NOT_READY_TO_SHIP` —— deliver 要求 READY_TO_SHIP
//! - 20118 `BIZ_PART_REPAIR_NOT_TRIGGERED` —— start_repair 要求 IN_PROCESS
//! - 20119 `BIZ_PART_NOT_DELETABLE` —— soft_delete 终态禁删（lifecycle 不直接用）
//! - 21420 `BIZ_DELIVERY_NOTE_LOCKED_PART` —— cancel 时 part 已挂送货单
//! - 40901 `VERSION_CONFLICT` —— 乐观锁失败
//!
//! 2026-09-22 D-6 重构：方法签名 `<R: PartRepoTrait>`（by-value；trait 已直接
//! `impl for &mut PgConnection`）。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::{NewPartEvent, TPart};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part::vo::PartOut;
use crate::shared::error::{AppError, code};

use super::super::dto_crud::{
    CancelRequest, CompleteRequest, DeliverRequest, ForceCompleteRequest, StartRepairRequest,
};
use super::PartService;

impl PartService {
    /// deliver (PR-B3 batch 级)：锚定 `t_part_batch.version`，状态机守卫读
    /// batch 当前状态 `READY_TO_SHIP → DELIVERED`。
    ///
    /// BREAKING CHANGE：DTO 新增 `batch_id` + `version`（前端从
    /// `GET /parts/by-serial/{serial_no}/part-batches` 取 batch.id + version）。
    pub async fn deliver<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: DeliverRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
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
        let batch = repo.find_batch_by_id(req.batch_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {} 不存在", req.batch_id),
            )
        })?;
        if batch.part_id != part_id {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {} 不属于 part {}", req.batch_id, part_id),
            ));
        }
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

    /// 取消工单。
    ///
    /// 守卫顺序（早 fail）：
    /// 1. part 不存在 / 已软删 → 20101
    /// 2. status 字符串非法 → 20104
    /// 3. status == CANCELLED → 20115
    /// 4. delivery_note_id 锁定 → 21420（**Finding D**）
    /// 5. status 不在 cancel 白名单 → 20103
    /// 6. part 翻转 → 同事务同步最近一条 source-status 批次
    pub async fn cancel<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: CancelRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        // 取完整 TPart（含 delivery_note_id）—— Finding D 要求 service 层守
        // 已挂送货单的 part 不能取消。
        let part: TPart = repo.get_part_detail(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
        })?;
        let from = PartStatus::from_str(&part.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("status 非法: {}", part.status),
            )
        })?;
        if from == PartStatus::CANCELLED {
            return Err(AppError::biz(
                code::BIZ_PART_ALREADY_CANCELLED,
                "工单已 CANCELLED",
            ));
        }
        // Finding D：cancel 锁定守护 — part 任一活跃批次已挂送货单 → 拒。
        // 2026-09-16 PR-2 瘦身（migration 027）：t_part.delivery_note_id 列已删，
        // 改查 t_part_batch.delivery_note_id 真相源。
        if repo.part_batch_has_active_on_delivery_note(part_id).await? {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_LOCKED_PART,
                format!("part {part_id} 存在活跃批次已挂送货单，禁 cancel"),
            ));
        }
        if !from.can_transition_to(PartStatus::CANCELLED) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "工单状态 {} 不允许取消（COMPLETED / CANCELLED 等终态不可取消）",
                    from.as_str()
                ),
            ));
        }
        let n = repo
            .mark_part_cancelled(part_id, part.version, current.id)
            .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突"),
            ));
        }
        // PR-B2 §4.2 cancel 改造：级联取消**全部活跃批次**（不只「最近一条
        // source-status」），单条 UPDATE 即覆盖。无活跃批次 → 影响行数 0，
        // 视为合法（新建工单未拆批场景）。
        //
        // 2026-10-01：该 bulk 写点走 status_gate 批量模式（并补上了它原先缺失的
        // `status NOT IN ('COMPLETED','CANCELLED')` 白名单 —— 原实现会把已完成
        // 批次一起拖成 CANCELLED），part → assembly 派生已在其内部完成。
        let _batches_cancelled = repo
            .cancel_all_active_batches_for_part(part_id, current.id)
            .await?;
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "CANCELLED",
            from_status: Some(from.as_str()),
            to_status: Some("CANCELLED"),
            batch_id: None,
            quantity: None,
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.reason.as_deref().or(req.note.as_deref()),
            created_by: Some(current.id),
        })
        .await?;
        // 重读走 TPartInspected（响应只需 PartOut 最小投影）。
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "cancel 后查不到"))?;
        Ok(PartOut::from(fresh))
    }

    pub async fn complete<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: CompleteRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
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
        let batch = repo.find_batch_by_id(req.batch_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {} 不存在", req.batch_id),
            )
        })?;
        if batch.part_id != part_id {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {} 不属于 part {}", req.batch_id, part_id),
            ));
        }
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
            .mark_batch_completed(batch.id, batch.version, current.id)
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
        part_id: i64,
        req: StartRepairRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
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
        let batch = repo.find_batch_by_id(req.batch_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {} 不存在", req.batch_id),
            )
        })?;
        if batch.part_id != part_id {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {} 不属于 part {}", req.batch_id, part_id),
            ));
        }
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

    /// 强推工单 + 所有活跃批次为 COMPLETED（force-complete 端点，2026-09-30 新增）。
    ///
    /// **MANAGER 单角色守卫**（明确不下放 Clerk 等其它角色 —— 强改逃生通道）；
    /// **完全绕状态机**：非 CANCELLED / 非 COMPLETED 状态均可被强推到 COMPLETED。
    ///
    /// 不走 OCC（force-complete 是逃生通道，依赖 SQL 行锁串行化）；
    /// `force_complete_all_batches_for_part` 单 SQL 强推 part 下所有非
    /// CANCELLED 活跃批次 → COMPLETED。
    ///
    /// 2026-10-01：该 bulk 写点已改走 `status_gate` 的批量模式，故
    /// `compute_part_target` 派生 `part.status='COMPLETED'`、级联 assembly
    /// 同步、终态序列号归档 / 释放**全部在同一事务内自动完成**，
    /// 本方法不再手工调 `sync_from_batch_change` / `clear_part_serial_no_when_completed`。
    ///
    /// 事件日志 `event_type='FORCE_COMPLETED'`（区别常规 COMPLETED），note 加
    /// `[FORCE]` 前缀以便审计追溯；WS 广播 `PART_FORCE_COMPLETED`。
    pub async fn force_complete<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ForceCompleteRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        // 1. MANAGER 单角色守卫（不下放 Clerk）。
        current.require_role(Role::Manager)?;
        // 2. 读 part（轻量投影，用于守卫 + 事件日志 drawing_code）。
        let part = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
        })?;
        let from = PartStatus::from_str(&part.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("status 非法: {}", part.status),
            )
        })?;
        // 3. 幂等拒绝：已 COMPLETED → 直接报错（避免重复强推副作用）。
        if from == PartStatus::COMPLETED {
            return Err(AppError::biz(
                code::BIZ_PART_ALREADY_COMPLETED,
                "工单已 COMPLETED",
            ));
        }
        // 4. 终态守护：已 CANCELLED → 拒（CANCELLED 不可被强推，语义对称 COMPLETED）。
        if from == PartStatus::CANCELLED {
            return Err(AppError::biz(
                code::BIZ_PART_ALREADY_CANCELLED,
                "工单已 CANCELLED",
            ));
        }
        // 5. 单 SQL 强推所有非 CANCELLED 活跃批次 → COMPLETED（绕 OCC）。
        //    2026-10-01：该 bulk 写点走 status_gate 的批量模式，因此
        //    part → assembly 派生（part 自动派生到 COMPLETED）与终态序列号
        //    归档 / 释放**都已在同一事务内完成**。原第 6/7 步
        //    （`sync_from_batch_change` + `clear_part_serial_no_when_completed`，
        //    两者都被 `let _ =` 静默吞掉）整体删除。
        let _n = repo
            .force_complete_all_batches_for_part(part_id, current.id)
            .await?;
        // 8. 事件日志：FORCE_COMPLETED 区分常规 COMPLETED；note 加 [FORCE] 前缀。
        let note_owned = req.note.unwrap_or_default();
        let prefixed_note = format!("[FORCE] {}", note_owned);
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "FORCE_COMPLETED",
            from_status: Some(from.as_str()),
            to_status: Some("COMPLETED"),
            batch_id: None,
            quantity: None,
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: Some(&prefixed_note),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "force-complete 后查不到"))?;
        Ok(PartOut::from(fresh))
    }
}

#[cfg(test)]
mod tests {
    //! `PartService::force_complete` 单元测试（2026-09-30 新增）。
    //!
    //! 覆盖 3 个早 fail 守卫：
    //! - `force_complete_rejects_clerk` —— MANAGER 单角色守卫，Clerk → 403
    //! - `force_complete_part_not_found` —— part 不存在 → 20101
    //! - `force_complete_rejects_already_completed` —— 幂等拒绝 → 20123
    //!
    //! 其余端到端路径（force 强推 + rollup + 事件日志 + WS 广播）由集成测试
    //! `tests/part/lifecycle.rs` 守护；mockall strict mode 要求每个被调用方法
    //! 都必须 expect，但未调用方法可不 expect。

    use chrono::NaiveDateTime;
    use mockall::predicate::*;

    use super::*;
    use crate::auth::rbac::{CurrentUser, Role};
    use crate::infra::snowflake::SnowflakeIdGenerator;
    use crate::modules::part::model::TPartInspected;
    use crate::modules::part::repo::MockPartRepoTrait;

    /// MANAGER 单角色测试用户（id=1）。
    fn current_manager() -> CurrentUser {
        CurrentUser {
            id: 1,
            username: "test-mgr".to_string(),
            roles: vec![Role::Manager],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    /// CLERK 角色测试用户（id=2；应被 force-complete 守卫拒）。
    fn current_clerk() -> CurrentUser {
        CurrentUser {
            id: 2,
            username: "test-clerk".to_string(),
            roles: vec![Role::Clerk],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    /// 测试用雪花 ID 生成器（instance_id=1，与生产对齐）。
    fn test_snowflake() -> std::sync::Arc<SnowflakeIdGenerator> {
        std::sync::Arc::new(SnowflakeIdGenerator::new(
            1_735_689_600_000, // 2025-01-01 UTC
            1,
        ))
    }

    /// 构造一个最小化的 `TPartInspected` 行（status 参数化）。
    fn sample_part_inspected(part_id: i64, status: &str) -> TPartInspected {
        let now =
            NaiveDateTime::parse_from_str("2026-09-30 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        TPartInspected {
            id: part_id,
            serial_no: Some(format!("SN-{part_id}")),
            name: format!("part-{part_id}"),
            drawing_no: format!("DWG-{part_id}"),
            status: status.to_string(),
            version: 1,
            quantity: 1,
            order_no: None,
            updated_at: now,
            updated_by: Some(1),
        }
    }

    #[tokio::test]
    async fn force_complete_rejects_clerk() {
        // Arrange：Clerk 角色 → 期望 force_complete 直接返回 FORBIDDEN，
        // 不会触发任何 repo 方法。
        let mock = MockPartRepoTrait::new();
        let snowflake = test_snowflake();

        // Act
        let err = PartService::force_complete(
            mock,
            &snowflake,
            42,
            ForceCompleteRequest { note: None },
            &current_clerk(),
        )
        .await
        .expect_err("force-complete Clerk 应被拒");

        // Assert
        assert_eq!(err.code(), code::FORBIDDEN);
    }

    #[tokio::test]
    async fn force_complete_part_not_found() {
        // Arrange：MANAGER 通过，但 get_part_inspected 返 None → BIZ_PART_NOT_FOUND
        // (20101)。repo 不应有其它方法被调用（早 fail 在 guard 后第一次 DB 读）。
        let mut mock = MockPartRepoTrait::new();
        mock.expect_get_part_inspected()
            .with(eq(42))
            .returning(|_| Ok(None));
        let snowflake = test_snowflake();

        // Act
        let err = PartService::force_complete(
            mock,
            &snowflake,
            42,
            ForceCompleteRequest { note: None },
            &current_manager(),
        )
        .await
        .expect_err("force-complete 不存在的 part 应报 not-found");

        // Assert
        assert_eq!(err.code(), code::BIZ_PART_NOT_FOUND);
    }

    #[tokio::test]
    async fn force_complete_rejects_already_completed() {
        // Arrange：MANAGER 通过 + get_part_inspected 返 COMPLETED 状态 →
        // BIZ_PART_ALREADY_COMPLETED (20123)。repo 只 expect get_part_inspected。
        let mut mock = MockPartRepoTrait::new();
        mock.expect_get_part_inspected()
            .with(eq(42))
            .returning(|id| Ok(Some(sample_part_inspected(id, "COMPLETED"))));
        let snowflake = test_snowflake();

        // Act
        let err = PartService::force_complete(
            mock,
            &snowflake,
            42,
            ForceCompleteRequest {
                note: Some("retry".to_string()),
            },
            &current_manager(),
        )
        .await
        .expect_err("force-complete 已 COMPLETED 工单应被幂等拒");

        // Assert
        assert_eq!(err.code(), code::BIZ_PART_ALREADY_COMPLETED);
    }
}
