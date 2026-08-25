//! part 域 lifecycle 业务逻辑
//!
//! 包含 4 个公开方法（工单状态机终态 / 返修翻转）：
//! - `deliver` —— READY_TO_SHIP → DELIVERED
//! - `complete` —— DELIVERED → COMPLETED
//! - `cancel` —— 5 状态白名单 → CANCELLED
//! - `start_repair` —— IN_PROCESS → REPAIRING
//!
//! 同步策略：每个生命周期方法在事务内同时翻转 `t_part` 与最近一条匹配的
//! `t_part_batch`（status 白名单匹配 + id DESC）。无 source-status 批次时仅翻
//! `t_part`（新建工单未拆批场景）。
//!
//! 错误码契约（与 `statemachine.rs` 对齐）：
//! - 20101 `BIZ_PART_NOT_FOUND` —— part 不存在 / 软删
//! - 20104 `BIZ_INVALID_VALUE` —— DB 中 status 字符串不在 enum 白名单
//! - 20103 `BIZ_INVALID_TRANSITION` —— 状态机白名单拒绝（cancel 时 COMPLETED/REPAIRING 等）
//! - 20114 `BIZ_PART_ALREADY_CANCELLED` —— 工单已 CANCELLED
//! - 20115 `BIZ_PART_NOT_DELIVERED` —— complete 要求 DELIVERED
//! - 20116 `BIZ_PART_NOT_READY_TO_SHIP` —— deliver 要求 READY_TO_SHIP
//! - 20117 `BIZ_PART_REPAIR_NOT_TRIGGERED` —— start_repair 要求 IN_PROCESS
//! - 20120 `BIZ_PART_NOT_DELETABLE` —— soft_delete 终态禁删（lifecycle 不直接用）
//! - 21420 `BIZ_DELIVERY_NOTE_LOCKED_PART` —— cancel 时 part 已挂送货单
//! - 40901 `VERSION_CONFLICT` —— 乐观锁失败

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::dto::PartOut;
use crate::modules::part::model::{NewPartEvent, TPart};
use crate::modules::part::repo::PartRepo;
use crate::modules::part::statemachine::PartStatus;
use crate::shared::error::{code, AppError};

use super::super::dto_crud::{CancelRequest, CompleteRequest, DeliverRequest, StartRepairRequest};
use super::PartService;

impl PartService {
    pub async fn deliver(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: DeliverRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
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
        if !from.can_transition_to(PartStatus::DELIVERED) {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_READY_TO_SHIP,
                format!(
                    "工单状态 {} 不允许 deliver（必须 READY_TO_SHIP）",
                    from.as_str()
                ),
            ));
        }
        let n = PartRepo::mark_part_delivered(&mut *conn, part_id, part.version, current.id).await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突"),
            ));
        }
        // 同步最近一条 READY_TO_SHIP 批次。无源批次 → 跳过（新建工单未拆批合法）。
        if let Some(b) =
            PartRepo::find_most_recent_batch_for_part(&mut *conn, part_id, "READY_TO_SHIP").await?
        {
            let bn = PartRepo::mark_batch_delivered(&mut *conn, b.id, b.version, current.id).await?;
            if bn == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("batch {} 版本冲突", b.id),
                ));
            }
        }
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "DELIVERED",
                from_status: Some("READY_TO_SHIP"),
                to_status: Some("DELIVERED"),
                batch_id: None,
                quantity: None,
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, "deliver 后查不到")
            })?;
        Ok(PartOut::from(fresh))
    }

    /// 取消工单。
    ///
    /// 守卫顺序（早 fail）：
    /// 1. part 不存在 / 已软删 → 20101
    /// 2. status 字符串非法 → 20104
    /// 3. status == CANCELLED → 20114
    /// 4. delivery_note_id 锁定 → 21420（**Finding D**）
    /// 5. status 不在 cancel 白名单 → 20103
    /// 6. part 翻转 → 同事务同步最近一条 source-status 批次
    pub async fn cancel(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: CancelRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        // 取完整 TPart（含 delivery_note_id）—— Finding D 要求 service 层守
        // 已挂送货单的 part 不能取消。
        let part: TPart = PartRepo::get_part_detail(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
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
        // Finding D：cancel 锁定守护 — part 已挂非 DRAFT 送货单 → 拒。
        if part.delivery_note_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_LOCKED_PART,
                format!(
                    "part {part_id} 已挂送货单（delivery_note_id={:?}），禁 cancel",
                    part.delivery_note_id
                ),
            ));
        }
        if !from.can_transition_to(PartStatus::CANCELLED) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "工单状态 {} 不允许取消（COMPLETED/REPAIRING 等不可取消）",
                    from.as_str()
                ),
            ));
        }
        let n = PartRepo::mark_part_cancelled(&mut *conn, part_id, part.version, current.id).await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突"),
            ));
        }
        // 同步最近一条 source-status 批次。无源批次 → 跳过。
        if let Some(b) =
            PartRepo::find_most_recent_batch_for_part(&mut *conn, part_id, from.as_str()).await?
        {
            let bn =
                PartRepo::mark_batch_cancelled(&mut *conn, b.id, b.version, current.id).await?;
            if bn == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("batch {} 版本冲突", b.id),
                ));
            }
        }
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
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
            },
        )
        .await?;
        // 重读走 TPartInspected（响应只需 PartOut 最小投影）。
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, "cancel 后查不到")
            })?;
        Ok(PartOut::from(fresh))
    }

    pub async fn complete(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: CompleteRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
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
        // Finding E：与 deliver / cancel 对齐走 `can_transition_to` 而非直接
        // 等值比较，保证状态机白名单是单一事实源。
        if !from.can_transition_to(PartStatus::COMPLETED) {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_DELIVERED,
                format!(
                    "工单当前状态 {} 无法 complete（必须 DELIVERED）",
                    from.as_str()
                ),
            ));
        }
        let n = PartRepo::mark_part_completed(&mut *conn, part_id, part.version, current.id).await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突"),
            ));
        }
        // 同步最近一条 DELIVERED 批次。无源批次 → 跳过。
        if let Some(b) =
            PartRepo::find_most_recent_batch_for_part(&mut *conn, part_id, "DELIVERED").await?
        {
            let bn =
                PartRepo::mark_batch_completed(&mut *conn, b.id, b.version, current.id).await?;
            if bn == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("batch {} 版本冲突", b.id),
                ));
            }
        }
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "COMPLETED",
                from_status: Some("DELIVERED"),
                to_status: Some("COMPLETED"),
                batch_id: None,
                quantity: None,
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, "complete 后查不到")
            })?;
        Ok(PartOut::from(fresh))
    }

    pub async fn start_repair(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: StartRepairRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
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
        // Finding E：与 deliver / cancel 对齐走 `can_transition_to` 而非直接
        // 等值比较，保证状态机白名单是单一事实源。
        if !from.can_transition_to(PartStatus::REPAIRING) {
            return Err(AppError::biz(
                code::BIZ_PART_REPAIR_NOT_TRIGGERED,
                format!(
                    "工单当前状态 {} 无法 start-repair（必须 IN_PROCESS）",
                    from.as_str()
                ),
            ));
        }
        let n = PartRepo::mark_part_repairing(&mut *conn, part_id, part.version, current.id).await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突"),
            ));
        }
        // 同步最近一条 IN_PROCESS 批次。无源批次 → 跳过。
        if let Some(b) =
            PartRepo::find_most_recent_batch_for_part(&mut *conn, part_id, "IN_PROCESS").await?
        {
            let bn =
                PartRepo::mark_batch_repairing(&mut *conn, b.id, b.version, current.id).await?;
            if bn == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("batch {} 版本冲突", b.id),
                ));
            }
        }
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "REPAIR_STARTED",
                from_status: Some("IN_PROCESS"),
                to_status: Some("REPAIRING"),
                batch_id: None,
                quantity: None,
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.reason.as_deref().or(req.note.as_deref()),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, "start-repair 后查不到")
            })?;
        Ok(PartOut::from(fresh))
    }
}