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

use super::PartService;
use crate::modules::part::dto_crud::{CancelRequest, ForceCompleteRequest};

impl PartService {
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
        // 批次一起拖成 CANCELLED），**父装配件**派生已在其内部完成。
        //
        // ⚠️ 2026-10-01 review 第 1 轮 B1：`t_part` 的派生在本路径上**被显式
        // 关掉**（`PartDerivation::KeepPartTerminalAsIs`）。上面 `mark_part_cancelled`
        // 才是 part 状态的主操作；若放任级联再按 min-progress 派生，「已完成批次
        // + 其余被批量取消」会算出 COMPLETED 并把用户的「作废工单」静默改回去
        // （接口 200、事件流水记 CANCELLED、界面显示已完成、父装配件被级联推成
        // COMPLETED）。SQL 层另有终态守卫兜底（`update_part_rollup`），两处都要在。
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
            .force_complete_all_batches_for_part(part_id, current.id, Some(snowflake.next_id()))
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
