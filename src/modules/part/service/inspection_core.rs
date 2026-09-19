//! part 域 inspection 流的三个共享核心（`to_ship_core` / `to_process_core` /
//! `to_inspection_core`）。
//!
//! 从 `inspection.rs` 拆出，原因是该文件触及 1000 行上限（见 docs/conventions.md）。
//! 三个 core 与 `inspection.rs` 里的薄 wrapper / 批量聚合器同属 `impl PartService`，
//! 分文件不改变可见性与调用方式（与 `worker_scan.rs` 同一模式）。

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::service::SyncOutcome;
use crate::modules::part::model::{NewPartEvent, TPartInspected};
use crate::modules::part::repo::PartRepo;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part_batch::model::TPartBatch;
use crate::modules::process_chain::repo::ProcessChainRepo;
use crate::shared::error::{AppError, code};

use super::super::dto::{PartOut, ToXxxOut};
use super::PartService;

impl PartService {
    /// to_ship 共享核心（被单件 / batch 端点共用）。
    ///
    /// 事务边界由 caller（handler / batch aggregator）保证；本方法只跑
    /// 业务校验 + repo 调用 + 状态机守卫。
    ///
    /// 流转：`t_part.status` & `t_part_batch.status` 同事务内 `INSPECTION` →
    /// `READY_TO_SHIP`（version 自增 + OCC 守卫），事件日志落 `t_part_event`。
    ///
    /// **部分通过（partial-pass split）**：`quantity < target.quantity` 时调
    /// `split_batch_for_partial_pass`：operated 部分（拆出的新批次，quantity =
    /// op_qty）走状态翻转，remainder 部分（原批次 quantity 减少）留在
    /// `INSPECTION`；返回 `new_batch_id = Some(remainder_id)`。整批操作
    /// （`quantity >= target.quantity`）不拆批，`new_batch_id = None`。
    ///
    /// # Errors
    /// - 20101 `BIZ_PART_NOT_FOUND`
    /// - 20103 `BIZ_INVALID_TRANSITION` —— 当前状态非 INSPECTION
    /// - 20109 `BIZ_PART_BATCH_NOT_FOUND` —— 找不到 INSPECTION 批次 / 多批歧义
    /// - 20111 `BIZ_PART_BATCH_INVALID_QUANTITY`
    /// - 40901 `VERSION_CONFLICT`
    pub async fn to_ship_core(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        batch_id: i64,
        expected_batch_version: i32,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        // 1. 读 part
        let part: TPartInspected = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
            })?;

        // 2. 状态机守卫：INSPECTION → READY_TO_SHIP
        let from = PartStatus::from_str(&part.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("part {} 状态非法: {}", part_id, part.status),
            )
        })?;
        if !from.can_transition_to(PartStatus::READY_TO_SHIP) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "part {} 当前状态 {} 不允许通过品检（必须先送检）",
                    part_id,
                    from.as_str()
                ),
            ));
        }

        // 3. 定位目标 INSPECTION 批次
        let bid_hint = Some(batch_id);
        let target: TPartBatch =
            match PartRepo::find_inprocess_batch_for_part(&mut *conn, part_id, bid_hint).await {
                Ok(Some(b)) => b,
                Ok(None) => {
                    return Err(AppError::biz(
                        code::BIZ_PART_BATCH_NOT_FOUND,
                        format!(
                            "part {part_id} 找不到 INSPECTION 批次（requested={:?}）",
                            bid_hint
                        ),
                    ));
                }
                Err(sqlx::Error::RowNotFound) => {
                    return Err(AppError::biz(
                        code::BIZ_PART_BATCH_NOT_FOUND,
                        "multiple INSPECTION batches; specify batch_id".to_string(),
                    ));
                }
                Err(e) => return Err(AppError::from(e)),
            };

        // 3.5 caller 侧乐观锁：锚定 batch 而非 part（part.version 会被同 part
        // 下其它批次的操作撞掉，锚 part 会产生假冲突）。
        Self::_assert_batch_version(&target, expected_batch_version)?;

        // 4. 部分通过拆批（如需要）
        let operated_quantity = quantity.unwrap_or(target.quantity);
        let (operated_id, operated_version, new_batch_id_out) =
            Self::_split_for_partial_op(&mut *conn, snowflake, &target, quantity, current).await?;

        // 5. UPDATE t_part_batch: INSPECTION → READY_TO_SHIP（OCC + 写 updated_by）
        let n = PartRepo::mark_batch_passed_inspection(
            &mut *conn,
            operated_id,
            operated_version,
            Some(current.id),
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("batch {operated_id} 版本冲突"),
            ));
        }

        // 6. 写 t_part_event 事件日志（无条件）
        let event_id = snowflake.next_id();
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: event_id,
                part_id,
                event_type: "STATUS_CHANGED",
                from_status: Some("INSPECTION"),
                to_status: Some("READY_TO_SHIP"),
                batch_id: Some(operated_id),
                quantity: Some(operated_quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: Some("batch to ship"),
                created_by: Some(current.id),
            },
        )
        .await?;

        // 7. PR-B2 batch → part rollup：翻 batch 后调 sync_from_batch_change，
        //    内部走 min-progress 规则（多条 INSPECTION 批次时 part 维持
        //    INSPECTION，单条时升 READY_TO_SHIP）；status 变化时级联调
        //    AssemblyService::sync_from_part_change 闭合链路。
        let synced = PartService::sync_from_batch_change(&mut *conn, part_id, current).await?;
        let synced_assembly_id = match synced {
            SyncOutcome::Changed(aid) => Some(aid),
            SyncOutcome::NoChange => None,
        };

        // 8. 重读返回
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished"))
            })?;
        Ok(ToXxxOut {
            part: PartOut::from(fresh),
            new_batch_id: new_batch_id_out,
            synced_assembly_id,
        })
    }

    /// to_process 共享核心（推荐需求 3）。
    ///
    /// 事务边界由 caller 保证；本方法跑业务校验 + repo 调用 + 状态机守卫。
    ///
    /// 流转：`t_part` & `t_part_batch` 同事务内 `INSPECTION → IN_PROCESS`（带
    /// `location='PRODUCTION_SHELF'` / `current_holder_id` / `next_process_id` 同步），
    /// 事件日志落 `t_part_event`。
    ///
    /// 部分通过拆批：同 `to_ship_core`（operated 走状态翻转，remainder 留在
    /// `INSPECTION`），返回 `new_batch_id = Some(remainder_id)`。
    ///
    /// # Errors
    /// - 20101 `BIZ_PART_NOT_FOUND`
    /// - 20103 `BIZ_INVALID_TRANSITION` —— 当前状态非 INSPECTION
    /// - 20104 `BIZ_INVALID_VALUE` —— shelf 不在 PRODUCTION 区 / 缺 shelf_id
    /// - 20109 `BIZ_PART_BATCH_NOT_FOUND`
    /// - 20111 `BIZ_PART_BATCH_INVALID_QUANTITY`
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20512 `BIZ_SHELF_INACTIVE`
    /// - 40901 `VERSION_CONFLICT`
    #[allow(clippy::too_many_arguments)]
    pub async fn to_process_core(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        shelf_id: i64,
        next_process_id: i64,
        note: Option<&str>,
        batch_id: i64,
        expected_batch_version: i32,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        // 1. 读 part
        let part: TPartInspected = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
            })?;
        // 2. 状态机守卫：必须 INSPECTION
        let from = PartStatus::from_str(&part.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("part {} 状态非法: {}", part_id, part.status),
            )
        })?;
        if !from.can_transition_to(PartStatus::IN_PROCESS) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "part {} 当前状态 {} 不允许品检打回（必须先送检到 INSPECTION）",
                    part_id,
                    from.as_str()
                ),
            ));
        }
        // 3. 校验 shelf（PRODUCTION 区 + active）
        Self::_validate_production_shelf_and_process(&mut *conn, shelf_id, next_process_id).await?;
        // 4. 定位目标 INSPECTION 批次
        let bid_hint = Some(batch_id);
        let target: TPartBatch =
            match PartRepo::find_inspection_batch_for_fail(&mut *conn, part_id, bid_hint).await {
                Ok(Some(b)) => b,
                Ok(None) => {
                    return Err(AppError::biz(
                        code::BIZ_PART_BATCH_NOT_FOUND,
                        format!("part {part_id} 找不到 INSPECTION 批次（hint={bid_hint:?}）"),
                    ));
                }
                Err(sqlx::Error::RowNotFound) => {
                    return Err(AppError::biz(
                        code::BIZ_PART_BATCH_NOT_FOUND,
                        "multiple INSPECTION batches; specify batch_id".to_string(),
                    ));
                }
                Err(e) => return Err(AppError::from(e)),
            };
        // 4.5 caller 侧乐观锁：锚定 batch 而非 part
        Self::_assert_batch_version(&target, expected_batch_version)?;
        // 5. 部分通过拆批
        let (operated_id, operated_version, new_batch_id_out) =
            Self::_split_for_partial_op(&mut *conn, snowflake, &target, quantity, current).await?;
        // 6. UPDATE t_part_batch: INSPECTION → IN_PROCESS + location/holder/process
        // PR-3 批次 step 化：解析 step_id（chain 内 process_id → step_id）。
        // 注意：part 存在性已在 step 1（PartRepo::get_part_inspected）确认，
        // 此处只需区分「part 软删 / 并发消失」与「part 未绑定工艺链」两种情形。
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT process_chain_id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .fetch_optional(&mut *conn)
        .await?;
        let chain_id = match row {
            None => {
                return Err(AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("part {part_id} 不存在或已软删"),
                ));
            }
            Some((None,)) => {
                return Err(AppError::biz(
                    code::BIZ_PROCESS_CHAIN_REQUIRED,
                    "to_process: part 必须已绑定工艺链",
                ));
            }
            Some((Some(cid),)) => cid,
        };
        let step_id =
            ProcessChainRepo::resolve_step_id_by_process(&mut *conn, chain_id, next_process_id)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                        format!(
                            "chain {} 内找不到 process_id={} 的活跃 step",
                            chain_id, next_process_id
                        ),
                    )
                })?;
        let n = PartRepo::mark_batch_failed_inspection(
            &mut *conn,
            operated_id,
            operated_version,
            shelf_id,
            Some(step_id),
            Some(current.id),
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("batch {operated_id} 版本冲突"),
            ));
        }
        // 7. 写事件日志
        let event_id = snowflake.next_id();
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: event_id,
                part_id,
                event_type: "INSPECTION_FAILED",
                from_status: Some("INSPECTION"),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(operated_id),
                quantity: Some(target.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note,
                created_by: Some(current.id),
            },
        )
        .await?;
        // 8. PR-B2 batch → part rollup：翻 batch 后调 sync_from_batch_change，
        //    内部走 min-progress 规则；status 变化时级联调
        //    AssemblyService::sync_from_part_change 闭合链路。
        let synced = PartService::sync_from_batch_change(&mut *conn, part_id, current).await?;
        let synced_assembly_id = match synced {
            SyncOutcome::Changed(aid) => Some(aid),
            SyncOutcome::NoChange => None,
        };
        // 9. 重读返回
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished"))
            })?;
        Ok(ToXxxOut {
            part: PartOut::from(fresh),
            new_batch_id: new_batch_id_out,
            synced_assembly_id,
        })
    }

    /// to_inspection 共享核心（被单件 / batch 端点共用）。
    ///
    /// 单步流程：`{PENDING, PROGRAMMING, IN_PROCESS}` → `INSPECTION`。
    /// **不再带 PASS/FAIL 分支**：通过品检由 client 另调 `to_ship_core`；打回
    /// 由 client 另调 `to_process_core`。两条路径各自独立事务 + 各自 OCC +
    /// 各自事件日志（to-XXX 模型把原 scan_inspect 同事务内的合并语义显式拆开）。
    ///
    /// 流转：
    /// 1. `t_part.status` & `t_part_batch.status` → `INSPECTION`，同步 `location =
    ///    'INSPECTION_SHELF'` / `current_holder_id = target_shelf.id`。
    /// 2. 写 `INSPECTED` 事件日志。
    ///
    /// 部分通过拆批：`quantity < target.quantity` 时调
    /// `split_batch_for_partial_pass`：operated 部分（拆出的新批次，quantity =
    /// op_qty）走状态翻转，remainder 部分留在 `{PENDING, PROGRAMMING,
    /// IN_PROCESS}` 待后续操作；返回 `new_batch_id = Some(remainder_id)`。
    ///
    /// # Errors
    /// - 20101 `BIZ_PART_NOT_FOUND`
    /// - 20103 `BIZ_INVALID_TRANSITION` —— 当前状态非 `{PENDING, PROGRAMMING,
    ///   IN_PROCESS}`；或 IN_PROCESS+WORKER；或 IN_PROCESS+非 PRODUCTION_SHELF
    /// - 20109 `BIZ_PART_BATCH_NOT_FOUND`
    /// - 20111 `BIZ_PART_BATCH_INVALID_QUANTITY`
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20511 `BIZ_SHELF_NOT_INSPECTION_ZONE`
    /// - 20512 `BIZ_SHELF_INACTIVE`
    /// - 40901 `VERSION_CONFLICT`
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：t_part 删 `current_holder_id` 列；
    /// IN_PROCESS 组合校验的「工人持有件 / 非生产架件拒绝」改读**目标批次**的
    /// `location` + `current_holder_id`（真相源在 t_part_batch）。原 step 4
    /// 重排至 step 5 之后，用 `target_batch.location` 直接判定（避免原版
    /// 「holder 是否命中 t_shelf」启发式歧义）。
    ///
    /// 2026-09-16 PR-2 行为收紧：IN_PROCESS + `target_batch.location IS NULL` 由
    /// 旧版「放行」改「拒绝」(`BIZ_INVALID_TRANSITION`)。理论上 IN_PROCESS 批次
    /// location 不为空（旧版放行是漏检 —— 任何 IN_PROCESS 批次必须挂在
    /// `PRODUCTION_SHELF` / `WORKER` / `OUTSOURCE_COMPANY` 之一，NULL 视为
    /// 数据完整性违规，宁可拒绝也不静默放行）。
    // 参数过多（9 > 7）。本函数聚合 part_id / shelf_id / batch_id / version /
    // quantity / note 等必要输入，与 `to_ship_core` 同形；将它们打包为
    // `ToInspectionCoreArgs` 结构体收益微薄、调用面广，重构 ROI 低，故豁免。
    #[allow(clippy::too_many_arguments)]
    pub async fn to_inspection_core(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        target_inspection_shelf_id: i64,
        batch_id: i64,
        expected_batch_version: i32,
        quantity: Option<i32>,
        note: Option<&str>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        // 1. 校验品检架（target_inspection_shelf）
        let target_shelf =
            Self::_validate_inspection_shelf(&mut *conn, target_inspection_shelf_id).await?;
        // 2. 读 part
        let part: TPartInspected = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
            })?;
        // 3. 状态机守卫：必须在 {PENDING, PROGRAMMING, IN_PROCESS}
        let from = PartStatus::from_str(&part.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("part {} 状态非法: {}", part_id, part.status),
            )
        })?;
        if !from.can_transition_to(PartStatus::INSPECTION) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("part {} 当前状态 {} 不允许送检", part_id, from.as_str()),
            ));
        }
        // 5. 定位目标批次（先于 IN_PROCESS 组合校验，以便直接读 target_batch.location）
        let target = Self::_resolve_scan_target_batch(&mut *conn, part_id, Some(batch_id)).await?;
        // 5.5 caller 侧乐观锁：锚定 batch 而非 part
        Self::_assert_batch_version(&target, expected_batch_version)?;
        // 4（PR-2 重排后）IN_PROCESS 组合校验：工人持有件 / 非生产架件拒绝
        //
        // 实现策略：t_part_batch.location 是单一权威值（'PRODUCTION_SHELF' /
        // 'INSPECTION_SHELF' / 'WORKER' / 'OUTSOURCE_COMPANY' / 'OFFICE'）。
        // IN_PROCESS 状态要求 location='PRODUCTION_SHELF'；否则按目标位置
        // 分类报错（工人持有 / 非生产架持有）。
        if from == PartStatus::IN_PROCESS {
            match target.location.as_deref() {
                Some("PRODUCTION_SHELF") => { /* 放行 */ }
                Some("WORKER") => {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_TRANSITION,
                        "工人持有件请先归还或送检".to_string(),
                    ));
                }
                Some(other) => {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_TRANSITION,
                        format!("不在生产架上，无法送检（batch.location={other}）"),
                    ));
                }
                None => {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_TRANSITION,
                        "batch.location 为空，无法送检".to_string(),
                    ));
                }
            }
        }
        // 6. 部分通过拆批
        let (operated_id, operated_version, new_batch_id_out) =
            Self::_split_for_partial_op(&mut *conn, snowflake, &target, quantity, current).await?;
        // 7. UPDATE t_part_batch: {PENDING, PROGRAMMING, IN_PROCESS} → INSPECTION
        //
        // 隐式多批次 rollup：前置状态守卫（step 3）已限定 from ∈ {PENDING, PROGRAMMING,
        // IN_PROCESS}，该状态下不可能存在 INSPECTION 批次，翻转 `t_part.status` 安全；
        // 翻 batch 后调 `PartService::sync_from_batch_change`（下方 step 8）按
        // min-progress 规则回填 part 派生列。
        let n = PartRepo::mark_batch_inspected(
            &mut *conn,
            operated_id,
            operated_version,
            target_shelf.id,
            Some(current.id),
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("batch {operated_id} 版本冲突"),
            ));
        }
        // 8. PR-B2 batch → part rollup：翻 batch 后调 sync_from_batch_change，
        //    内部走 min-progress 规则；status 变化时级联调
        //    AssemblyService::sync_from_part_change 闭合链路。
        let synced = PartService::sync_from_batch_change(&mut *conn, part_id, current).await?;
        let synced_assembly_id = match synced {
            SyncOutcome::Changed(aid) => Some(aid),
            SyncOutcome::NoChange => None,
        };
        // 9. 写 INSPECTED 事件日志
        let event_id = snowflake.next_id();
        let note_text = match from {
            PartStatus::PENDING => format!("送检：来自待下发 → 品检架 {}", target_shelf.code),
            PartStatus::PROGRAMMING => format!("送检：来自编程中 → 品检架 {}", target_shelf.code),
            PartStatus::IN_PROCESS => format!("送检：来自生产架 → 品检架 {}", target_shelf.code),
            _ => format!("送检 → 品检架 {}", target_shelf.code),
        };
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: event_id,
                part_id,
                event_type: "INSPECTED",
                from_status: Some(from.as_str()),
                to_status: Some("INSPECTION"),
                batch_id: Some(operated_id),
                quantity: Some(target.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: note.or(Some(&note_text)),
                created_by: Some(current.id),
            },
        )
        .await?;
        // 10. 重读返回
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished"))
            })?;
        Ok(ToXxxOut {
            part: PartOut::from(fresh),
            new_batch_id: new_batch_id_out,
            synced_assembly_id,
        })
    }
}
