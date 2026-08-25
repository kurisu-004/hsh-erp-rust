//! part 域业务逻辑
//!
//! 对应 Python myERP/service/part_service.py（及 _<d>_*.py helper）。
//! 实施约定：方法签名接收 `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## Phase F（pass_inspection 批量送检）
//! - `PartService::pass_inspection_core`：单件共享核心（被单件 / batch 端点共用）
//! - `PartService::pass_inspection`：单件公开 service fn（handler 调用）
//! - `PartService::batch_pass_inspection`：批量（per-item 独立事务边界外的循环，
//!   失败 item 收集到 `failed` 并继续）
//!
//! ## Phase F2（scan-inspect / fail-inspection）
//! - `PartService::scan_inspect_core`：单件共享核心（被单件 / batch 端点共用）
//! - `PartService::scan_inspect`：单件公开 service fn（handler 调用）
//! - `PartService::batch_scan_inspect`：批量（per-item 独立事务边界外的循环，
//!   失败 item 收集到 `failed` 并继续）
//! - `PartService::fail_inspection_core`：单件品检打回（推荐需求 3）
//! - `PartService::fail_inspection`：fail_inspection_core 薄包装
//!
//! ## 错误码契约（Phase F2 新增 / 复用）
//! - 20511 `BIZ_SHELF_NOT_INSPECTION_ZONE` —— target_inspection_shelf.zone ≠ 'INSPECTION'
//! - 20512 `BIZ_SHELF_INACTIVE` —— target_inspection_shelf.is_active = false
//!
//! ## 错误码契约
//! - 20101 `BIZ_PART_NOT_FOUND` —— part 不存在
//! - 20103 `BIZ_INVALID_TRANSITION` —— 当前状态非 INSPECTION（双重校验）
//! - 20109 `BIZ_PART_BATCH_NOT_FOUND` —— 找不到指定 INSPECTION 批次 / 多个 INSPECTION 批次无 hint
//! - 20111 `BIZ_PART_BATCH_INVALID_QUANTITY` —— 拆分数量非法（≤0 或 ≥ batch_quantity）
//! - 40001 `VALIDATION_ERROR` —— 批量入参校验失败（items 为空 / 超过上限）
//! - 40901 `VERSION_CONFLICT` —— 乐观锁失败（已被并发修改）

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::{NewPartEvent, TPartInspected};
use crate::modules::part::repo::PartRepo;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part_batch::model::TPartBatch;
use crate::modules::shelf::model::TShelf;
use crate::modules::shelf::repo::ShelfRepo;
use crate::modules::worker::repo::WorkerRepo;
use crate::modules::worker_pool::dto::WorkerScanEvent;
use crate::shared::error::{code, AppError};

use super::dto::{
    BatchPassFailure, BatchPassInspectionOut, BatchPassInspectionRequest, BatchScanInspectFailure,
    BatchScanInspectOut, BatchScanInspectRequest, FailInspectionRequest, PartOut, ScanDecision,
    ScanInspectRequest, WorkerScanCoreOut, WorkerScanRequest,
};

/// 批量端点单次请求最大 item 数（handler/service 双层校验）。
pub const BATCH_PASS_INSPECTION_MAX_ITEMS: usize = 200;

/// 批量 scan-inspect 单次请求最大 item 数（与 batch-pass-inspection 对齐）。
pub const BATCH_SCAN_INSPECT_MAX_ITEMS: usize = 200;

pub struct PartService;

impl PartService {
    /// 单件共享核心：被单件 / batch 端点共用。
    ///
    /// 事务边界由 caller（handler / batch aggregator）保证；本方法只跑
    /// 业务校验 + repo 调用 + 状态机守卫。
    ///
    /// 流转：`t_part.status` & `t_part_batch.status` 同事务内 `INSPECTION` →
    /// `READY_TO_SHIP`（version 自增 + OCC 守卫），事件日志落 `t_part_event`。
    ///
    /// **部分通过（partial-pass split）留待后续 PR**：当前实现 `quantity <
    /// target.quantity` 时直接返回 `20111 BIZ_PART_BATCH_INVALID_QUANTITY`。
    /// `split_batch_for_partial_pass` repo 方法已在 repo 层就绪，但 service
    /// 层故意不暴露 —— 该路径需要额外的事务一致性 + 失败回滚策略文档，
    /// 与本次 PR 的 INSPECTION → READY_TO_SHIP 单状态翻转解耦。
    ///
    /// # Errors
    /// - `20101` `BIZ_PART_NOT_FOUND` —— part 不存在 / 软删
    /// - `20103` `BIZ_INVALID_TRANSITION` —— 当前状态非 INSPECTION
    /// - `20109` `BIZ_PART_BATCH_NOT_FOUND` —— 找不到 INSPECTION 批次 / 多个
    ///   INSPECTION 批次但 caller 未指定 `batch_id`（歧义）
    /// - `20111` `BIZ_PART_BATCH_INVALID_QUANTITY` —— `quantity ≤ 0` 或
    ///   `quantity > target.quantity`（partial-pass 未启用）
    /// - `40901` `VERSION_CONFLICT` —— 乐观锁失败（t_part 或 t_part_batch）
    pub async fn pass_inspection_core(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        batch_id: Option<i64>,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        // 1. 读 part（最小投影，含 status / version / quantity 等 pass_inspection
        //    必需字段）。
        let part: TPartInspected =
            PartRepo::get_part_inspected(&mut *conn, part_id).await?.ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
            })?;

        // 2. 状态机守卫：INSPECTION → READY_TO_SHIP。状态字符串不在 enum 白
        //    名单 → BIZ_INVALID_VALUE；当前状态不允许迁移 → BIZ_INVALID_TRANSITION。
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
                    "part {} 当前状态 {} 不允许品检通过",
                    part_id,
                    from.as_str()
                ),
            ));
        }

        // 3. 定位目标 INSPECTION 批次。多个 INSPECTION 批次 + caller 未给
        //    `batch_id` 时，repo 返回 `sqlx::Error::RowNotFound`，本步翻译为
        //    `BIZ_PART_BATCH_NOT_FOUND`。
        let bid_hint = batch_id;
        let target: TPartBatch = match PartRepo::find_inprocess_batch_for_part(
            &mut *conn,
            part_id,
            bid_hint,
        )
        .await
        {
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
                // ≥2 个 INSPECTION 批次且 caller 未给 batch_id：歧义。
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    "multiple INSPECTION batches; specify batch_id".to_string(),
                ));
            }
            Err(e) => return Err(AppError::from(e)),
        };

        // 4. 数量守卫。当前 PR 不支持 partial-pass，quantity 必须等于整批
        //    quantity（缺省=整批）。quantity ≤ 0 或 > target.quantity → 20111。
        let target_quantity = quantity.unwrap_or(target.quantity);
        if target_quantity <= 0 || target_quantity > target.quantity {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_INVALID_QUANTITY,
                format!(
                    "quantity {} 非法；batch {} 余量 {}（partial-pass 留待后续 PR）",
                    target_quantity, target.id, target.quantity
                ),
            ));
        }

        // 5. UPDATE t_part_batch: INSPECTION → READY_TO_SHIP（OCC + 写 updated_by）。
        let n = PartRepo::mark_batch_passed_inspection(
            &mut *conn,
            target.id,
            target.version,
            Some(current.id),
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("batch {} 版本冲突", target.id),
            ));
        }

        // 6. 写 t_part_event 事件日志（无条件）。事件 id 走雪花生成（App 侧生成，
        //    DB 默认列对齐）。这一步**必须在多轮 rollup 守卫之前**：即便工单因其它
        //    INSPECTION 批次残留而保留 INSPECTION 状态，本次批次通过的事实仍要
        //    留痕（与 batch 自身的 status 翻转同步落库），否则审计日志会丢失
        //    multi-batch 场景下的 batch-pass 事件。
        let event_id = snowflake.next_id();
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: event_id,
                part_id,
                event_type: "STATUS_CHANGED",
                from_status: Some("INSPECTION"),
                to_status: Some("READY_TO_SHIP"),
                batch_id: Some(target.id),
                quantity: Some(target_quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: Some("batch pass inspection"),
                created_by: Some(current.id),
            },
        )
        .await?;

        // 多轮 rollup 守卫：仅当无其它 INSPECTION 批次时才翻 `t_part.status`。
        // 否则其它批次仍在 INSPECTION 状态，工单必须保留 INSPECTION
        // （对齐 Python `_rollup_part_status` 的 `least(batches.status)` 语义）。
        let other_inprocess =
            PartRepo::count_other_inprocess_batches(&mut *conn, part_id).await?;
        if other_inprocess > 0 {
            // 还有别的 INSPECTION 批次存在，跳过 part.status 翻转；
            // 批次状态由 `t_part_batch` 自身记录（含 updated_by），事件日志
            // 已在上一步无条件写入。工单保留 INSPECTION，返回读到的旧 `PartOut`。
            let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
                .await?
                .ok_or_else(|| {
                    AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished"))
                })?;
            return Ok(PartOut::from(fresh));
        }

        // 7. UPDATE t_part: 同步工单状态（OCC + 写 updated_by）。这一步解决
        //    `PartOut.status` 在响应中仍为旧值 INSPECTION 的契约漏洞。
        let n = PartRepo::mark_part_passed_inspection(
            &mut *conn,
            part_id,
            part.version,
            Some(current.id),
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突"),
            ));
        }

        // 8. 重新读最新 part 状态返回（t_part.version+1，status=READY_TO_SHIP）。
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished"))
            })?;
        Ok(PartOut::from(fresh))
    }

    /// 单件公开 service fn（handler 调用）。
    ///
    /// 当前为 pass_inspection_core 的薄包装；保留独立方法名以便后续在
    /// 单件路径上插入与批量路径不同的横切逻辑（如单条额外的审计事件）。
    pub async fn pass_inspection(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        batch_id: Option<i64>,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        Self::pass_inspection_core(conn, snowflake, part_id, batch_id, quantity, current).await
    }

    /// 批量 —— 每个 item 在 caller 的外层事务内执行（共享 `&mut PgConnection`）。
    ///
    /// 失败 item **不中断** 后续 item；失败原因收集到 `failed` Vec（含 part_id
    /// 与错误码、错误消息）。调用方按需决定是否对 failed item 做整体回滚
    /// （当前契约：成功与失败同提交）。
    ///
    /// 入参校验：
    /// - `items` 为空 → `40001 VALIDATION_ERROR`
    /// - `items.len() > BATCH_PASS_INSPECTION_MAX_ITEMS` → `40001 VALIDATION_ERROR`
    pub async fn batch_pass_inspection(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: BatchPassInspectionRequest,
        current: &CurrentUser,
    ) -> Result<BatchPassInspectionOut, AppError> {
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        if req.items.len() > BATCH_PASS_INSPECTION_MAX_ITEMS {
            return Err(AppError::validation(format!(
                "items 数量 {} 超过上限 {}",
                req.items.len(),
                BATCH_PASS_INSPECTION_MAX_ITEMS
            )));
        }

        let mut passed: Vec<PartOut> = Vec::new();
        let mut failed: Vec<BatchPassFailure> = Vec::new();
        for item in req.items {
            // 解析 batch_id：None 跳过；非数字字符串直接以 `40001 VALIDATION_ERROR`
            // 计入 `failed`（不让 `pass_inspection_core` 把歧义
            // BIZ_PART_BATCH_NOT_FOUND 当成"找不到 INSPECTION 批次"上报）。
            let bid = match item.batch_id.as_deref() {
                None => None,
                Some(s) => match s.parse::<i64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        failed.push(BatchPassFailure {
                            part_id: item.part_id,
                            code: code::VALIDATION_ERROR,
                            message: format!("batch_id '{s}' is not a numeric id"),
                        });
                        continue;
                    }
                },
            };
            match Self::pass_inspection_core(
                conn, snowflake, item.part_id, bid, item.quantity, current,
            )
            .await
            {
                Ok(p) => passed.push(p),
                Err(e) => failed.push(BatchPassFailure {
                    part_id: item.part_id,
                    code: e.code(),
                    message: format!("{e}"),
                }),
            }
        }
        Ok(BatchPassInspectionOut { passed, failed })
    }

    /// 校验品检架：必须存在、未软删、is_active=true、zone='INSPECTION'。
    ///
    /// 错误码：
    /// - 20501 `BIZ_SHELF_NOT_FOUND`：不存在 / 已软删
    /// - 20512 `BIZ_SHELF_INACTIVE`：is_active=false
    /// - 20511 `BIZ_SHELF_NOT_INSPECTION_ZONE`：zone ≠ 'INSPECTION'
    async fn _validate_inspection_shelf(
        conn: &mut PgConnection,
        shelf_id: i64,
    ) -> Result<TShelf, AppError> {
        let shelf = ShelfRepo::get_by_id(&mut *conn, shelf_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {shelf_id} 不存在"),
                )
            })?;
        if !shelf.is_active {
            return Err(AppError::biz(
                code::BIZ_SHELF_INACTIVE,
                format!("shelf {} (id={}) 已停用", shelf.code, shelf.id),
            ));
        }
        if shelf.zone != "INSPECTION" {
            return Err(AppError::biz(
                code::BIZ_SHELF_NOT_INSPECTION_ZONE,
                format!(
                    "shelf {} (id={}) 不在 INSPECTION 区（zone={}）",
                    shelf.code, shelf.id, shelf.zone
                ),
            ));
        }
        Ok(shelf)
    }

    /// 校验生产架 + 下一道工序映射（用于 fail-inspection 与 scan-inspect FAIL 分支）。
    ///
    /// 仅校验 shelf 在 PRODUCTION 区且 active；next_process_id 与 shelf 映射
    /// 校验通过 `t_shelf_process` 表 SQL（详 shelf 域后续 PR；本 PR 仅校验
    /// shelf 存在 + zone + active，next_process_id 由 caller 传入，校验留待
    /// 后续 shelf 域 PR 实施）。
    ///
    /// 错误码：
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20512 `BIZ_SHELF_INACTIVE`
    /// - 20104 `BIZ_INVALID_VALUE`：zone ≠ 'PRODUCTION'
    async fn _validate_production_shelf_and_process(
        conn: &mut PgConnection,
        shelf_id: i64,
        next_process_id: i64,
    ) -> Result<TShelf, AppError> {
        let shelf = ShelfRepo::get_by_id(&mut *conn, shelf_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {shelf_id} 不存在"),
                )
            })?;
        if !shelf.is_active {
            return Err(AppError::biz(
                code::BIZ_SHELF_INACTIVE,
                format!("shelf {} (id={}) 已停用", shelf.code, shelf.id),
            ));
        }
        if shelf.zone != "PRODUCTION" {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "shelf {} (id={}) 不在 PRODUCTION 区（zone={}）",
                    shelf.code, shelf.id, shelf.zone
                ),
            ));
        }
        // next_process_id 与 shelf 映射的强校验（spec §需求 3 步骤 2 标注
        // "必须与 shelf_id 对应映射"）属于 shelf 域职责。当前 shelf 域占位，
        // 未暴露 `t_shelf_process` 映射查询 fn；本 PR 仅接受 caller 传入的
        // next_process_id 并写入 DB，跨 shelf 映射校验留待 shelf 域 PR。
        let _ = next_process_id;
        Ok(shelf)
    }

    /// 定位 scan-inspect 目标批次（白名单 `{PENDING, PROGRAMMING, IN_PROCESS}`）。
    ///
    /// 多批次歧义 → 20109；未找到 → 20109；显式 batch_id 但不属于 part → 20109。
    ///
    /// 错误码：
    /// - 20109 `BIZ_PART_BATCH_NOT_FOUND`
    async fn _resolve_scan_target_batch(
        conn: &mut PgConnection,
        part_id: i64,
        batch_id: Option<i64>,
    ) -> Result<TPartBatch, AppError> {
        match PartRepo::find_scan_target_batch(&mut *conn, part_id, batch_id).await {
            Ok(Some(b)) => Ok(b),
            Ok(None) => Err(AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("part {part_id} 找不到候选批次（hint={:?}）", batch_id),
            )),
            Err(sqlx::Error::RowNotFound) => Err(AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                "多个候选批次；请显式指定 batch_id".to_string(),
            )),
            Err(e) => Err(AppError::from(e)),
        }
    }

    /// 部分通过：拆出目标 quantity 的新批次（继承 location/holder/next_process）。
    ///
    /// 返回新批次（caller 后续应在新批次上做状态翻转）。
    /// - quantity=None 或 quantity==target.quantity：直接返回 target（不拆）
    /// - 0 < quantity < target.quantity：调 `split_batch_for_partial_pass`，返回新批次
    /// - quantity <= 0 或 > target.quantity：抛 20111
    async fn _split_batch_if_needed(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        target: TPartBatch,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<TPartBatch, AppError> {
        let q = match quantity {
            None => return Ok(target),
            Some(q) if q == target.quantity => return Ok(target),
            Some(q) => q,
        };
        if q <= 0 || q > target.quantity {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_INVALID_QUANTITY,
                format!("quantity {q} 非法；batch {} 余量 {}", target.id, target.quantity),
            ));
        }
        let new_batch_id = snowflake.next_id();
        PartRepo::split_batch_for_partial_pass(
            &mut *conn,
            new_batch_id,
            target.id,
            target.version,
            target.part_id,
            q,
            Some(current.id),
        )
        .await?;
        // 重读新批次
        let new_batch: TPartBatch = PartRepo::find_scan_target_batch(&mut *conn, target.part_id, Some(new_batch_id))
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, format!("新批次 {new_batch_id} vanished"))
            })?;
        Ok(new_batch)
    }

    /// fail-inspection 共享核心（推荐需求 3）。
    ///
    /// 事务边界由 caller 保证；本方法跑业务校验 + repo 调用 + 状态机守卫。
    ///
    /// 流转：`t_part` & `t_part_batch` 同事务内 `INSPECTION → IN_PROCESS`（带
    /// `location='PRODUCTION_SHELF'` / `current_holder_id` / `next_process_id` 同步），
    /// 事件日志落 `t_part_event`。
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
    pub async fn fail_inspection_core(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        shelf_id: i64,
        next_process_id: i64,
        note: Option<&str>,
        batch_id: Option<i64>,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        // 1. 读 part
        let part: TPartInspected =
            PartRepo::get_part_inspected(&mut *conn, part_id).await?.ok_or_else(|| {
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
        let target: TPartBatch = match PartRepo::find_inspection_batch_for_fail(
            &mut *conn, part_id, batch_id,
        )
        .await
        {
            Ok(Some(b)) => b,
            Ok(None) => return Err(AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("part {part_id} 找不到 INSPECTION 批次（hint={batch_id:?}）"),
            )),
            Err(sqlx::Error::RowNotFound) => return Err(AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                "multiple INSPECTION batches; specify batch_id".to_string(),
            )),
            Err(e) => return Err(AppError::from(e)),
        };
        // 5. 拆批（如需要）
        let target = Self::_split_batch_if_needed(&mut *conn, snowflake, target, quantity, current).await?;
        // 6. UPDATE t_part_batch: INSPECTION → IN_PROCESS + location/holder/process
        let n = PartRepo::mark_batch_failed_inspection(
            &mut *conn, target.id, target.version, shelf_id, next_process_id, Some(current.id),
        ).await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, format!("batch {} 版本冲突", target.id)));
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
                batch_id: Some(target.id),
                quantity: Some(target.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note,
                created_by: Some(current.id),
            },
        ).await?;
        // 8. 多轮 rollup 守卫：还有别的 INSPECTION 批次？保留工单 INSPECTION 状态
        let other_inprocess = PartRepo::count_other_inprocess_batches(&mut *conn, part_id).await?;
        if other_inprocess > 0 {
            let fresh = PartRepo::get_part_inspected(&mut *conn, part_id).await?
                .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished")))?;
            return Ok(PartOut::from(fresh));
        }
        // 9. UPDATE t_part: 同步工单状态
        let n = PartRepo::mark_part_failed_inspection(
            &mut *conn, part_id, part.version, shelf_id, next_process_id, Some(current.id),
        ).await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, format!("part {part_id} 版本冲突")));
        }
        // 10. 重读返回
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id).await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished")))?;
        Ok(PartOut::from(fresh))
    }

    /// fail-inspection 薄包装（handler 调用）。
    pub async fn fail_inspection(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: FailInspectionRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        let shelf_id: i64 = req.shelf_id.parse().map_err(|_| {
            AppError::biz(code::BIZ_INVALID_VALUE, format!("shelf_id '{}' is not a numeric id", req.shelf_id))
        })?;
        let next_process_id: i64 = req.next_process_id.parse().map_err(|_| {
            AppError::biz(code::BIZ_INVALID_VALUE, format!("next_process_id '{}' is not a numeric id", req.next_process_id))
        })?;
        let batch_id = req.batch_id.as_deref().and_then(|s| s.parse().ok());
        Self::fail_inspection_core(
            &mut *conn, snowflake, part_id, shelf_id, next_process_id,
            req.note.as_deref(), batch_id, req.quantity, current,
        ).await
    }

    /// scan-inspect 共享核心（被单件 / batch 端点共用）。
    ///
    /// 两步流程（仿 myERP `service/part.py:4178-4210`）：
    /// 1. **搬到 INSPECTION**：`t_part.status = 'INSPECTION'` + `location = 'INSPECTION_SHELF'` +
    ///    `current_holder_id = target_shelf.id`（OCC）；同步 `t_part_batch` 状态；
    ///    写 `INSPECTED` 事件日志。
    /// 2. **decision 分流**：PASS → 调 `pass_inspection_core`（同事务复用）；
    ///    FAIL → 调 `fail_inspection_core`（同事务复用）。
    ///
    /// # Errors
    /// - 20101 `BIZ_PART_NOT_FOUND`
    /// - 20103 `BIZ_INVALID_TRANSITION` —— 当前状态非 `{PENDING, PROGRAMMING, IN_PROCESS}`;
    ///   或 IN_PROCESS+WORKER；或 IN_PROCESS+非 PRODUCTION_SHELF
    /// - 20104 `BIZ_INVALID_VALUE` —— FAIL 缺 shelf_id / next_process_id
    /// - 20109 `BIZ_PART_BATCH_NOT_FOUND`
    /// - 20111 `BIZ_PART_BATCH_INVALID_QUANTITY`
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20511 `BIZ_SHELF_NOT_INSPECTION_ZONE`
    /// - 20512 `BIZ_SHELF_INACTIVE`
    /// - 40901 `VERSION_CONFLICT`
    #[allow(clippy::too_many_arguments)]
    pub async fn scan_inspect_core(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        target_inspection_shelf_id: i64,
        decision: ScanDecision,
        shelf_id: Option<i64>,
        next_process_id: Option<i64>,
        note: Option<&str>,
        batch_id: Option<i64>,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        // 0. FAIL 必填校验（早期失败）
        if decision == ScanDecision::FAIL && (shelf_id.is_none() || next_process_id.is_none()) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "FAIL 决策必须填齐 shelf_id 和 next_process_id",
            ));
        }
        // 1. 校验品检架（target_inspection_shelf）
        let target_shelf = Self::_validate_inspection_shelf(&mut *conn, target_inspection_shelf_id).await?;
        // 2. 读 part
        let part: TPartInspected =
            PartRepo::get_part_inspected(&mut *conn, part_id).await?.ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
            })?;
        // 3. 状态机守卫：必须在 {PENDING, PROGRAMMING, IN_PROCESS}
        let from = PartStatus::from_str(&part.status).ok_or_else(|| {
            AppError::biz(code::BIZ_INVALID_VALUE, format!("part {} 状态非法: {}", part_id, part.status))
        })?;
        if !from.can_transition_to(PartStatus::INSPECTION) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "part {} 当前状态 {} 不允许扫码快捷品检",
                    part_id, from.as_str()
                ),
            ));
        }
        // 4. IN_PROCESS 组合校验：工人持有件 / 非生产架件拒绝
        //
        // 实现策略：`t_part.current_holder_id` 在 DB 层同时承载「worker 持有」与
        // 「shelf 持有」两种语义；区分方式是看该 id 是否能命中 `t_shelf`：
        // - ShelfRepo::get_by_id 返回 Some → 是 shelf
        // - 返回 None → 是 worker（v1 myERP 也是用此启发式区分）
        if from == PartStatus::IN_PROCESS
            && let Some(holder_id) = part.current_holder_id
        {
            match ShelfRepo::get_by_id(&mut *conn, holder_id).await? {
                None => {
                    // holder 不在 t_shelf → 视为 worker 持有
                    return Err(AppError::biz(
                        code::BIZ_INVALID_TRANSITION,
                        "工人持有件请先归还或送检".to_string(),
                    ));
                }
                Some(holder_shelf) if holder_shelf.zone != "PRODUCTION" => {
                    // holder 是 shelf 但不是生产架 → 拒绝
                    return Err(AppError::biz(
                        code::BIZ_INVALID_TRANSITION,
                        "不在生产架上，无法快捷品检".to_string(),
                    ));
                }
                Some(_) => { /* holder 是 PRODUCTION 区货架 → 放行 */ }
            }
        }
        // 5. 定位目标批次
        let target = Self::_resolve_scan_target_batch(&mut *conn, part_id, batch_id).await?;
        // 6. 拆批（如需要）
        let target = Self::_split_batch_if_needed(&mut *conn, snowflake, target, quantity, current).await?;
        // 7. 第一步：搬到 INSPECTION
        // 隐式多批次 rollup：本步不显式调用 `count_other_inprocess_batches`，
        // 因为前置状态守卫（step 3）已限定 from ∈ {PENDING, PROGRAMMING, IN_PROCESS}，
        // 该状态下不可能存在 INSPECTION 批次，翻转 `t_part.status` 安全。
        // 7a. UPDATE t_part
        let n = PartRepo::mark_part_inspected(
            &mut *conn, part_id, part.version, target_shelf.id, Some(current.id),
        ).await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, format!("part {part_id} 版本冲突")));
        }
        // 7b. UPDATE t_part_batch
        let n = PartRepo::mark_batch_inspected(
            &mut *conn, target.id, target.version, target_shelf.id, Some(current.id),
        ).await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, format!("batch {} 版本冲突", target.id)));
        }
        // 7c. 写 INSPECTED 事件日志
        let event_id = snowflake.next_id();
        let note_text = match from {
            PartStatus::PENDING => format!(
                "扫码快捷品检：来自待下发 → 品检架 {}",
                target_shelf.code
            ),
            PartStatus::PROGRAMMING => format!(
                "扫码快捷品检：来自编程中 → 品检架 {}",
                target_shelf.code
            ),
            PartStatus::IN_PROCESS => format!(
                "扫码快捷品检：来自生产架 → 品检架 {}",
                target_shelf.code
            ),
            _ => format!("扫码快捷品检 → 品检架 {}", target_shelf.code),
        };
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: event_id,
                part_id,
                event_type: "INSPECTED",
                from_status: Some(from.as_str()),
                to_status: Some("INSPECTION"),
                batch_id: Some(target.id),
                quantity: Some(target.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: Some(&note_text),
                created_by: Some(current.id),
            },
        ).await?;
        // 8. 第二步：decision 分流
        match decision {
            ScanDecision::PASS => {
                // 复用 pass_inspection_core（已存在于本 service）
                Self::pass_inspection_core(
                    &mut *conn, snowflake, part_id, Some(target.id), Some(target.quantity), current,
                ).await
            }
            ScanDecision::FAIL => {
                // 复用 fail_inspection_core（已实现于本 task）
                Self::fail_inspection_core(
                    &mut *conn, snowflake, part_id,
                    shelf_id.unwrap(), next_process_id.unwrap(),
                    note, Some(target.id), Some(target.quantity), current,
                ).await
            }
        }
    }

    /// scan-inspect 薄包装（handler 调用）。
    pub async fn scan_inspect(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ScanInspectRequest,
        current: &CurrentUser,
    ) -> Result<PartOut, AppError> {
        let target_inspection_shelf_id: i64 = req.target_inspection_shelf_id.parse().map_err(|_| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("target_inspection_shelf_id '{}' is not a numeric id", req.target_inspection_shelf_id),
            )
        })?;
        let shelf_id = req.shelf_id.as_deref().and_then(|s| s.parse().ok());
        let next_process_id = req.next_process_id.as_deref().and_then(|s| s.parse().ok());
        let batch_id = req.batch_id.as_deref().and_then(|s| s.parse().ok());
        Self::scan_inspect_core(
            &mut *conn, snowflake, part_id, target_inspection_shelf_id,
            req.decision, shelf_id, next_process_id,
            req.note.as_deref(), batch_id, req.quantity, current,
        ).await
    }

    /// 批量 scan-inspect：每个 item 在 caller 的外层事务内执行（共享 `&mut PgConnection`）。
    ///
    /// 失败 item **不中断** 后续 item；失败原因收集到 `failed` Vec。
    ///
    /// 与 `batch_pass_inspection` 同形；额外校验：
    /// - `items.is_empty()` → 40001
    /// - `items.len() > BATCH_SCAN_INSPECT_MAX_ITEMS` → 40001
    /// - 每个 item 独立 `scan_inspect_core`（共享 `target_inspection_shelf_id` 校验
    ///   在循环外一次性做，避免重复 query）
    pub async fn batch_scan_inspect(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: BatchScanInspectRequest,
        current: &CurrentUser,
    ) -> Result<BatchScanInspectOut, AppError> {
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        if req.items.len() > BATCH_SCAN_INSPECT_MAX_ITEMS {
            return Err(AppError::validation(format!(
                "items 数量 {} 超过上限 {}",
                req.items.len(),
                BATCH_SCAN_INSPECT_MAX_ITEMS
            )));
        }
        let target_shelf_id: i64 = req.target_inspection_shelf_id.parse().map_err(|_| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("target_inspection_shelf_id '{}' is not a numeric id", req.target_inspection_shelf_id),
            )
        })?;
        // 共享品检架校验（一次性）
        Self::_validate_inspection_shelf(&mut *conn, target_shelf_id).await?;

        let mut submitted: Vec<PartOut> = Vec::new();
        let mut failed: Vec<BatchScanInspectFailure> = Vec::new();
        for item in req.items {
            // 解析嵌套 ID；解析失败直接进 failed（与 batch-pass-inspection 同形）
            let bid = match item.batch_id.as_deref() {
                None => None,
                Some(s) => match s.parse::<i64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        failed.push(BatchScanInspectFailure {
                            part_id: item.part_id,
                            code: code::VALIDATION_ERROR,
                            message: format!("batch_id '{s}' is not a numeric id"),
                        });
                        continue;
                    }
                },
            };
            let shelf_id = match item.shelf_id.as_deref() {
                None => None,
                Some(s) => match s.parse::<i64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        failed.push(BatchScanInspectFailure {
                            part_id: item.part_id,
                            code: code::VALIDATION_ERROR,
                            message: format!("shelf_id '{s}' is not a numeric id"),
                        });
                        continue;
                    }
                },
            };
            let next_process_id = match item.next_process_id.as_deref() {
                None => None,
                Some(s) => match s.parse::<i64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        failed.push(BatchScanInspectFailure {
                            part_id: item.part_id,
                            code: code::VALIDATION_ERROR,
                            message: format!("next_process_id '{s}' is not a numeric id"),
                        });
                        continue;
                    }
                },
            };
            let decision = item.decision.unwrap_or(ScanDecision::PASS);
            match Self::scan_inspect_core(
                &mut *conn, snowflake, item.part_id, target_shelf_id,
                decision, shelf_id, next_process_id,
                item.note.as_deref(), bid, item.quantity, current,
            )
            .await
            {
                Ok(p) => submitted.push(p),
                Err(e) => failed.push(BatchScanInspectFailure {
                    part_id: item.part_id,
                    code: e.code(),
                    message: format!("{e}"),
                }),
            }
        }
        Ok(BatchScanInspectOut { submitted, failed })
    }

    /// worker-scan 共享核心（被单件端点 `POST /parts/worker-scan` 调用，Task 8）。
    ///
    /// 两分支：
    /// - `RETURNED`：worker 把持有件放回生产架（同 admin_remove 语义，但走扫码台
    ///   + worker 自查路径）；要求 shelf ∈ PRODUCTION 区 + active；shelf ↔
    ///   next_process_id 在 `t_shelf_process` 必须有映射（20507 NOT_MAPPED）；
    ///   切 holder worker → shelf（OCC）+ 写 `RETURNED_TO_SHELF` 事件。
    /// - `INSPECTED`：worker 把持有件直接送检（INSPECTION → INSPECTION + 状态机
    ///   IN_PROCESS → INSPECTION）；要求 target shelf ∈ INSPECTION 区 + active；
    ///   状态机校验 → mark_*_inspected（OCC）+ 写 `SENT_TO_INSPECTION` 事件。
    ///
    /// 两条路径都先定位 worker 持有的 IN_PROCESS+WORKER 批次
    /// （`find_worker_held_batch_for_part`），多批次歧义 / 没持有 → 20114
    /// BIZ_PART_BATCH_NOT_HELD_BY_WORKER。
    ///
    /// **本方法不负责 refill**：handler 在 commit 之前紧接着调
    /// `WorkerPoolService::refill_for_worker`（同事务）。这样 scan 与 refill 共享
    /// 一个原子事务（OM-6 决议），避免扫描放回 → refill 抢批中间被并发抢走
    /// 同批的竞争窗口。
    ///
    /// `WorkerScanEvent` 是 unit enum（`Copy`），所以 `req` 按值传（caller 的
    /// DTO `req.clone()` 不再需要）。
    ///
    /// # Errors
    /// - 20101 `BIZ_PART_NOT_FOUND` —— serial_no 不存在
    /// - 20103 `BIZ_INVALID_TRANSITION` —— part 当前状态不允许（INSPECTED 分支）
    /// - 20104 `BIZ_INVALID_VALUE` —— 非法 part 状态
    /// - 20114 `BIZ_PART_BATCH_NOT_HELD_BY_WORKER` —— worker 没持有 / 多批歧义
    /// - 20201 `BIZ_WORKER_NOT_FOUND` —— badge_code 未注册
    /// - 20202 `BIZ_WORKER_INACTIVE` —— worker 已停用
    /// - 20206 `BIZ_WORKER_NO_WORK_TYPE` —— worker 未分配工种（refill 会在下一步报 20905）
    /// - 20501 `BIZ_SHELF_NOT_FOUND` —— shelf 不存在 / 非 PRODUCTION / target 非 INSPECTION
    /// - 20507 `BIZ_SHELF_PROCESS_NOT_MAPPED` —— RETURNED 时 shelf ↔ process 未映射
    /// - 20511 `BIZ_SHELF_NOT_INSPECTION_ZONE` —— target_inspection_shelf.zone ≠ 'INSPECTION'
    /// - 40001 `VALIDATION_ERROR` —— next_process_id / target_inspection_shelf_id 缺 / 非法
    /// - 40901 `VERSION_CONFLICT` —— 乐观锁失败
    #[allow(clippy::too_many_lines)]
    pub async fn worker_scan_event(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: WorkerScanRequest,
        current: &CurrentUser,
    ) -> Result<WorkerScanCoreOut, AppError> {
        // 1. shelf 校验（worker-scan shelf 必须 PRODUCTION 区 active）
        let shelf = ShelfRepo::get_by_id_zone(&mut *conn, req.shelf_id, "PRODUCTION")
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {} 不存在或非 PRODUCTION 区", req.shelf_id),
                )
            })?;
        // 2. 反查 worker
        let worker = WorkerRepo::get_by_badge_code(&mut *conn, &req.badge_code, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORKER_NOT_FOUND,
                    format!("badge_code {} 未注册", req.badge_code),
                )
            })?;
        if !worker.is_active {
            return Err(AppError::biz(
                code::BIZ_WORKER_INACTIVE,
                format!("worker {} 已停用", worker.id),
            ));
        }
        let work_type_id = worker.work_type_id.ok_or_else(|| {
            AppError::biz(
                code::BIZ_WORKER_NO_WORK_TYPE,
                format!("worker {} 未分配工种", worker.id),
            )
        })?;
        // 3. 定位 part
        let part = PartRepo::get_by_serial(&mut *conn, &req.serial_no, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("serial_no {} 不存在", req.serial_no),
                )
            })?;
        // 4. 定位 batch（worker 持有 IN_PROCESS+WORKER）
        let bid_hint = req.batch_id.as_deref().and_then(|s| s.parse().ok());
        let batch = match PartRepo::find_worker_held_batch_for_part(
            &mut *conn,
            part.id,
            worker.id,
            bid_hint,
        )
        .await
        {
            Ok(Some(b)) => b,
            Ok(None) => {
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_NOT_HELD_BY_WORKER,
                    format!("worker {} 未持有 part {} 的活跃批次", worker.id, part.id),
                ));
            }
            Err(sqlx::Error::RowNotFound) => {
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_NOT_HELD_BY_WORKER,
                    "multiple IN_PROCESS batches held by this worker; specify batch_id"
                        .to_string(),
                ));
            }
            Err(e) => return Err(AppError::from(e)),
        };
        // 5. event_type 分支
        let event_type_str: &'static str;
        match req.event_type {
            WorkerScanEvent::RETURNED => {
                // RETURNED 必须传 next_process_id
                let next_pid: i64 = req
                    .next_process_id
                    .as_deref()
                    .ok_or_else(|| AppError::validation("RETURNED 必须传 next_process_id"))?
                    .parse()
                    .map_err(|_| AppError::validation("next_process_id 非法"))?;
                // shelf ↔ process 映射校验（JOIN t_shelf_process）
                let maps: bool = sqlx::query_scalar!(
                    r#"SELECT EXISTS(
                        SELECT 1 FROM t_shelf_process
                        WHERE shelf_id = $1 AND process_id = $2
                    ) AS "exists!""#,
                    req.shelf_id,
                    next_pid,
                )
                .fetch_one(&mut *conn)
                .await?;
                if !maps {
                    return Err(AppError::biz(
                        code::BIZ_SHELF_PROCESS_NOT_MAPPED,
                        format!("shelf {} 不映射到工序 {}", req.shelf_id, next_pid),
                    ));
                }
                // 切 holder worker → shelf（OCC）
                let n = PartRepo::mark_batch_returned(
                    &mut *conn,
                    batch.id,
                    batch.version,
                    req.shelf_id,
                    next_pid,
                    Some(current.id),
                )
                .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                let n = PartRepo::mark_part_returned(
                    &mut *conn,
                    part.id,
                    part.version,
                    req.shelf_id,
                    next_pid,
                    Some(current.id),
                )
                .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                PartRepo::insert_part_event(
                    &mut *conn,
                    NewPartEvent {
                        id: snowflake.next_id(),
                        part_id: part.id,
                        event_type: "RETURNED_TO_SHELF",
                        from_status: Some("IN_PROCESS"),
                        to_status: Some("IN_PROCESS"),
                        batch_id: Some(batch.id),
                        quantity: Some(batch.quantity),
                        drawing_code: Some(&part.drawing_no),
                        badge_code: Some(&worker.badge_code),
                        note: None,
                        created_by: Some(current.id),
                    },
                )
                .await?;
                event_type_str = "WORKER_SCAN_RETURNED";
            }
            WorkerScanEvent::INSPECTED => {
                // INSPECTED 必须传 target_inspection_shelf_id
                let target_id: i64 = req
                    .target_inspection_shelf_id
                    .as_deref()
                    .ok_or_else(|| {
                        AppError::validation("INSPECTED 必须传 target_inspection_shelf_id")
                    })?
                    .parse()
                    .map_err(|_| AppError::validation("target_inspection_shelf_id 非法"))?;
                let target = ShelfRepo::get_active_by_id(&mut *conn, target_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(code::BIZ_SHELF_NOT_FOUND, "target shelf 不存在")
                    })?;
                if target.zone != "INSPECTION" {
                    return Err(AppError::biz(
                        code::BIZ_SHELF_NOT_INSPECTION_ZONE,
                        format!("target shelf zone={} 非 INSPECTION", target.zone),
                    ));
                }
                // 防御性：即使 req.shelf_id 已在 scope 内，target 也需校验
                // （SHELF_ACCOUNT 用户的 shelf_ids 是手填白名单）。
                if !current.can_access_shelf(target_id) {
                    return Err(AppError::biz(
                        code::SHELF_MISMATCH,
                        format!("无权限访问 target shelf {}", target_id),
                    ));
                }
                // 状态机：IN_PROCESS → INSPECTION
                let from = PartStatus::from_str(&part.status).ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "非法 part 状态")
                })?;
                if !from.can_transition_to(PartStatus::INSPECTION) {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_TRANSITION,
                        format!(
                            "part {} 当前状态 {} 不允许送检",
                            part.id,
                            from.as_str()
                        ),
                    ));
                }
                // 切 holder worker → target_shelf + 状态 IN_PROCESS → INSPECTION（OCC）
                let n = PartRepo::mark_batch_inspected(
                    &mut *conn,
                    batch.id,
                    batch.version,
                    target_id,
                    Some(current.id),
                )
                .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                let n = PartRepo::mark_part_inspected(
                    &mut *conn,
                    part.id,
                    part.version,
                    target_id,
                    Some(current.id),
                )
                .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                PartRepo::insert_part_event(
                    &mut *conn,
                    NewPartEvent {
                        id: snowflake.next_id(),
                        part_id: part.id,
                        event_type: "SENT_TO_INSPECTION",
                        from_status: Some("IN_PROCESS"),
                        to_status: Some("INSPECTION"),
                        batch_id: Some(batch.id),
                        quantity: Some(batch.quantity),
                        drawing_code: Some(&part.drawing_no),
                        badge_code: Some(&worker.badge_code),
                        note: None,
                        created_by: Some(current.id),
                    },
                )
                .await?;
                event_type_str = "WORKER_SCAN_INSPECTED";
            }
        }
        let _ = work_type_id; // 暂未直接使用，避免编译警告（refill 在 handler 下一步）
        let _ = shelf; // 仅用于 zone 校验，存根避免未用告警
        Ok(WorkerScanCoreOut {
            worker_id: worker.id,
            part_id: part.id,
            batch_id: batch.id,
            event_type: event_type_str.to_string(),
        })
    }
}