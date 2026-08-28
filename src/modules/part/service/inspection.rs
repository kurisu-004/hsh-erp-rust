//! part 域 to-XXX 流业务逻辑（to_ship / to_inspection / to_process 流）。
//!
//! 对应 Python myERP/service/part_service.py 中三个 inspection 流的实现（已统一为
//! to_ship / to_inspection / to_process 模型）。
//! 实施约定：方法签名接收 `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## to_ship 流（part 通过品检 / 部分通过拆批）
//! - `PartService::to_ship_core`：单件共享核心（被单件 / batch 端点共用）
//! - `PartService::to_ship`：单件公开 service fn（handler 调用）
//! - `PartService::batch_to_ship`：批量（per-item 失败不中断）
//!
//! ## to_inspection 流（送检 / 单步；不再带 PASS/FAIL 分支）
//! - `PartService::to_inspection_core`：单件共享核心（`{PENDING, PROGRAMMING, IN_PROCESS}` → `INSPECTION`）
//! - `PartService::to_inspection`：单件公开 service fn
//! - `PartService::batch_to_inspection`：批量
//!
//! ## to_process 流（品检打回 / 选下一工序）
//! - `PartService::to_process_core`：单件共享核心（`INSPECTION` → `IN_PROCESS`）
//! - `PartService::to_process`：薄包装
//!
//! ## 错误码契约（Phase F2 新增 / 复用）
//! - 20511 `BIZ_SHELF_NOT_INSPECTION_ZONE` —— target_inspection_shelf.zone ≠ 'INSPECTION'
//! - 20512 `BIZ_SHELF_INACTIVE` —— target_inspection_shelf.is_active = false
//!
//! ## 错误码契约
//! - 20101 `BIZ_PART_NOT_FOUND` —— part 不存在
//! - 20103 `BIZ_INVALID_TRANSITION` —— 当前状态非合法 to-XXX 起点
//! - 20104 `BIZ_INVALID_VALUE` —— 入参值非法
//! - 20109 `BIZ_PART_BATCH_NOT_FOUND` —— 找不到指定批次 / 多个候选批次无 hint
//! - 20111 `BIZ_PART_BATCH_INVALID_QUANTITY` —— 拆分数量非法（仅 ≤0；> batch_quantity 等价于整批操作）
//! - 40001 `VALIDATION_ERROR` —— 批量入参校验失败（items 为空 / 超过上限 / batch_id 解析失败）
//! - 40901 `VERSION_CONFLICT` —— 乐观锁失败（已被并发修改）

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::service::{AssemblyService, SyncOutcome};
use crate::modules::part::model::{NewPartEvent, TPartInspected};
use crate::modules::part::repo::PartRepo;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part_batch::model::TPartBatch;
use crate::modules::shelf::model::TShelf;
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{code, AppError};

use super::super::dto::{
    BatchOpFailure, BatchToInspectionRequest, BatchToShipRequest, BatchToXxxOut, PartOut,
    ToInspectionRequest, ToProcessRequest, ToShipRequest, ToXxxOut,
};
use super::PartService;

/// 批量 to-ship 单次请求最大 item 数（handler/service 双层校验）。
pub const BATCH_TO_SHIP_MAX_ITEMS: usize = 200;

/// 批量 to-inspection 单次请求最大 item 数（与 batch-to-ship 对齐）。
pub const BATCH_TO_INSPECTION_MAX_ITEMS: usize = 200;

impl PartService {
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

    /// 校验生产架 + 下一道工序映射（用于 to_process）。
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
        // next_process_id 与 shelf 映射的强校验属于 shelf 域职责。本 PR 仅
        // 接受 caller 传入的 next_process_id 并写入 DB，跨 shelf 映射校验
        // 留待 shelf 域 PR。
        let _ = next_process_id;
        Ok(shelf)
    }

    /// 定位 to-inspection 目标批次（白名单 `{PENDING, PROGRAMMING, IN_PROCESS}`）。
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

    /// 按 `id` 反查 `t_part_batch` 行（用于批量端点 item 反查 part_id）。
    ///
    /// 与 `PartRepo::*_batch_for_part` 的差异：本方法只按 `id` + 未软删定位,
    /// 不限制 status / part_id —— caller 拿到 `TPartBatch` 后用 `part_id` /
    /// `status` 字段自行派发到对应的 `to_*_core`。
    ///
    /// 委托给 [`PartRepo::find_batch_by_id`]（编译期 `sqlx::query_as!`）,
    /// 走 repo 层统一约定,避免 service 内联 SQL。
    async fn _lookup_batch_by_id(
        conn: &mut PgConnection,
        batch_id: i64,
    ) -> Result<TPartBatch, AppError> {
        PartRepo::find_batch_by_id(&mut *conn, batch_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("batch {batch_id} 不存在"),
                )
            })
    }

    /// 部分通过拆批（to_ship / to_inspection / to_process 共享）。
    ///
    /// 返回 `(operated_id, operated_version, new_batch_id_out)`：
    /// - `op_qty < target.quantity`（> 0）：调 `split_batch_for_partial_pass`，
    ///   operated = 新批次（status 沿用源状态；version=0），new_batch_id_out =
    ///   `Some(target.id)`（remainder 留在源状态）。
    /// - `op_qty >= target.quantity`（缺省 / 等于 / 大于 0）：不拆批，
    ///   operated = target，new_batch_id_out = `None`。
    /// - `op_qty <= 0` 或 `op_qty > target.quantity`：抛 20111。
    async fn _split_for_partial_op(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        target: &TPartBatch,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<(i64, i32, Option<i64>), AppError> {
        let op_qty = quantity.unwrap_or(target.quantity);
        if op_qty <= 0 {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_INVALID_QUANTITY,
                format!(
                    "quantity {op_qty} 非法；batch {} 余量 {}",
                    target.id, target.quantity
                ),
            ));
        }
        if op_qty < target.quantity {
            let new_batch_id = snowflake.next_id();
            PartRepo::split_batch_for_partial_pass(
                &mut *conn,
                new_batch_id,
                target.id,
                target.version,
                target.part_id,
                op_qty,
                &target.status,
                Some(current.id),
            )
            .await?;
            Ok((new_batch_id, 0, Some(target.id)))
        } else {
            Ok((target.id, target.version, None))
        }
    }

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
        batch_id: Option<i64>,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        // 1. 读 part
        let part: TPartInspected =
            PartRepo::get_part_inspected(&mut *conn, part_id).await?.ok_or_else(|| {
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
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    "multiple INSPECTION batches; specify batch_id".to_string(),
                ));
            }
            Err(e) => return Err(AppError::from(e)),
        };

        // 4. 部分通过拆批（如需要）
        let operated_quantity = quantity.unwrap_or(target.quantity);
        let (operated_id, operated_version, new_batch_id_out) = Self::_split_for_partial_op(
            &mut *conn,
            snowflake,
            &target,
            quantity,
            current,
        )
        .await?;

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

        // 7. 多轮 rollup 守卫：仅当无其它 INSPECTION 批次时才翻 t_part.status
        let other_inprocess =
            PartRepo::count_other_inprocess_batches(&mut *conn, part_id).await?;
        if other_inprocess > 0 {
            let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
                .await?
                .ok_or_else(|| {
                    AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished"))
                })?;
            return Ok(ToXxxOut {
                part: PartOut::from(fresh),
                new_batch_id: new_batch_id_out,
                synced_assembly_id: None,
            });
        }

        // 8. UPDATE t_part: 同步工单状态
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
        // —— 父装配件自动同步：part.status 翻 READY_TO_SHIP 后回流父 assembly ——
        // 仅当 mark_part_passed_inspection 真正执行（即无其它 INSPECTION 批次）才触发；
        // rollup guard 早返回路径跳过同步。
        let synced = AssemblyService::sync_from_part_change(&mut *conn, part_id, current).await?;
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

    /// to_ship 薄包装（handler 调用）。
    ///
    /// 当前为 to_ship_core 的薄包装；保留独立方法名以便后续在单件路径上
    /// 插入与批量路径不同的横切逻辑（如单条额外的审计事件）。
    pub async fn to_ship(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ToShipRequest,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        let batch_id = req.batch_id.as_deref().and_then(|s| s.parse().ok());
        Self::to_ship_core(
            &mut *conn,
            snowflake,
            part_id,
            batch_id,
            req.quantity,
            current,
        )
        .await
    }

    /// 批量 to-ship：每个 item 在 caller 的外层事务内执行（共享 `&mut PgConnection`）。
    ///
    /// 失败 item **不中断** 后续 item；失败原因收集到 `failed` Vec。
    ///
    /// 入参校验：
    /// - `items` 为空 → `40001 VALIDATION_ERROR`
    /// - `items.len() > BATCH_TO_SHIP_MAX_ITEMS` → `40001 VALIDATION_ERROR`
    /// - item.batch_id 解析失败 → 推 `BatchOpFailure { batch_id: 0 (sentinel), code: 40001, message: "batch_id 解析失败" }`
    pub async fn batch_to_ship(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: BatchToShipRequest,
        current: &CurrentUser,
    ) -> Result<BatchToXxxOut, AppError> {
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        if req.items.len() > BATCH_TO_SHIP_MAX_ITEMS {
            return Err(AppError::validation(format!(
                "items 数量 {} 超过上限 {}",
                req.items.len(),
                BATCH_TO_SHIP_MAX_ITEMS
            )));
        }

        let mut submitted: Vec<ToXxxOut> = Vec::new();
        let mut failed: Vec<BatchOpFailure> = Vec::new();
        for (idx, item) in req.items.iter().enumerate() {
            // 解析 batch_id：失败 → 推 sentinel failure + continue（不让 to_ship_core
            // 把歧义 BIZ_PART_BATCH_NOT_FOUND 当成"找不到 INSPECTION 批次"上报）。
            let parsed_bid: i64 = match item.batch_id.parse::<i64>() {
                Ok(n) => n,
                Err(_) => {
                    failed.push(BatchOpFailure {
                        batch_id: 0,
                        code: code::VALIDATION_ERROR,
                        message: format!(
                            "batch_id '{}' 解析失败",
                            item.batch_id
                        ),
                    });
                    continue;
                }
            };
            // 反查 batch（拿 part_id + 校验存在 + 校验未软删）
            let target = match Self::_lookup_batch_by_id(&mut *conn, parsed_bid).await {
                Ok(t) => t,
                Err(e) => {
                    failed.push(BatchOpFailure {
                        batch_id: parsed_bid,
                        code: e.code(),
                        message: format!("{e}"),
                    });
                    continue;
                }
            };
            // per-item savepoint：失败 item 回滚部分写入，不影响后续 item（参考 batch_create_parts）
            use sqlx::AssertSqlSafe;
            let sp_name = format!("batch_to_ship_item_{idx}");
            sqlx::raw_sql(AssertSqlSafe(format!("SAVEPOINT {sp_name}")))
                .execute(&mut *conn)
                .await?;
            match Self::to_ship_core(
                &mut *conn,
                snowflake,
                target.part_id,
                Some(target.id),
                item.quantity,
                current,
            )
            .await
            {
                Ok(o) => {
                    sqlx::raw_sql(AssertSqlSafe(format!("RELEASE SAVEPOINT {sp_name}")))
                        .execute(&mut *conn)
                        .await?;
                    submitted.push(o);
                }
                Err(e) => {
                    sqlx::raw_sql(AssertSqlSafe(format!("ROLLBACK TO SAVEPOINT {sp_name}")))
                        .execute(&mut *conn)
                        .await?;
                    failed.push(BatchOpFailure {
                        batch_id: target.id,
                        code: e.code(),
                        message: format!("{e}"),
                    });
                }
            }
        }
        // 父装配件同步已由各 to_ship_core 在 savepoint 内调用 sync_from_part_change 完成。
        Ok(BatchToXxxOut { submitted, failed })
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
        batch_id: Option<i64>,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
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
        // 5. 部分通过拆批
        let (operated_id, operated_version, new_batch_id_out) = Self::_split_for_partial_op(
            &mut *conn, snowflake, &target, quantity, current,
        )
        .await?;
        // 6. UPDATE t_part_batch: INSPECTION → IN_PROCESS + location/holder/process
        let n = PartRepo::mark_batch_failed_inspection(
            &mut *conn,
            operated_id,
            operated_version,
            shelf_id,
            next_process_id,
            Some(current.id),
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, format!("batch {operated_id} 版本冲突")));
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
        // 8. 多轮 rollup 守卫：还有别的 INSPECTION 批次？保留工单 INSPECTION 状态
        let other_inprocess = PartRepo::count_other_inprocess_batches(&mut *conn, part_id).await?;
        if other_inprocess > 0 {
            let fresh = PartRepo::get_part_inspected(&mut *conn, part_id).await?
                .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished")))?;
            return Ok(ToXxxOut {
                part: PartOut::from(fresh),
                new_batch_id: new_batch_id_out,
                synced_assembly_id: None,
            });
        }
        // 9. UPDATE t_part: 同步工单状态
        let n = PartRepo::mark_part_failed_inspection(
            &mut *conn, part_id, part.version, shelf_id, next_process_id, Some(current.id),
        ).await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, format!("part {part_id} 版本冲突")));
        }
        // —— 父装配件自动同步：part.status 翻 IN_PROCESS 后回流父 assembly ——
        // 仅当 mark_part_failed_inspection 真正执行（即无其它 INSPECTION 批次）才触发；
        // rollup guard 早返回路径跳过同步。
        let synced = AssemblyService::sync_from_part_change(&mut *conn, part_id, current).await?;
        let synced_assembly_id = match synced {
            SyncOutcome::Changed(aid) => Some(aid),
            SyncOutcome::NoChange => None,
        };
        // 10. 重读返回
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id).await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished")))?;
        Ok(ToXxxOut {
            part: PartOut::from(fresh),
            new_batch_id: new_batch_id_out,
            synced_assembly_id,
        })
    }

    /// to_process 薄包装（handler 调用）。
    pub async fn to_process(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ToProcessRequest,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        let shelf_id: i64 = req.shelf_id.parse().map_err(|_| {
            AppError::biz(code::BIZ_INVALID_VALUE, format!("shelf_id '{}' is not a numeric id", req.shelf_id))
        })?;
        let next_process_id: i64 = req.next_process_id.parse().map_err(|_| {
            AppError::biz(code::BIZ_INVALID_VALUE, format!("next_process_id '{}' is not a numeric id", req.next_process_id))
        })?;
        let batch_id = req.batch_id.as_deref().and_then(|s| s.parse().ok());
        Self::to_process_core(
            &mut *conn, snowflake, part_id, shelf_id, next_process_id,
            req.note.as_deref(), batch_id, req.quantity, current,
        ).await
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
    pub async fn to_inspection_core(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        target_inspection_shelf_id: i64,
        batch_id: Option<i64>,
        quantity: Option<i32>,
        note: Option<&str>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        // 1. 校验品检架（target_inspection_shelf）
        let target_shelf =
            Self::_validate_inspection_shelf(&mut *conn, target_inspection_shelf_id).await?;
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
                    "part {} 当前状态 {} 不允许送检",
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
                        "不在生产架上，无法送检".to_string(),
                    ));
                }
                Some(_) => { /* holder 是 PRODUCTION 区货架 → 放行 */ }
            }
        }
        // 5. 定位目标批次
        let target = Self::_resolve_scan_target_batch(&mut *conn, part_id, batch_id).await?;
        // 6. 部分通过拆批
        let (operated_id, operated_version, new_batch_id_out) = Self::_split_for_partial_op(
            &mut *conn, snowflake, &target, quantity, current,
        )
        .await?;
        // 7. UPDATE t_part_batch: {PENDING, PROGRAMMING, IN_PROCESS} → INSPECTION
        //
        // 隐式多批次 rollup：本步不显式调用 `count_other_inprocess_batches`，
        // 因为前置状态守卫（step 3）已限定 from ∈ {PENDING, PROGRAMMING, IN_PROCESS}，
        // 该状态下不可能存在 INSPECTION 批次，翻转 `t_part.status` 安全。
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
        // 8. UPDATE t_part
        let n = PartRepo::mark_part_inspected(
            &mut *conn, part_id, part.version, target_shelf.id, Some(current.id),
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, format!("part {part_id} 版本冲突")));
        }
        // —— 父装配件自动同步：part.status 翻 INSPECTION 后回流父 assembly ——
        let synced = AssemblyService::sync_from_part_change(&mut *conn, part_id, current).await?;
        let synced_assembly_id = match synced {
            SyncOutcome::Changed(aid) => Some(aid),
            SyncOutcome::NoChange => None,
        };
        // 9. 写 INSPECTED 事件日志
        let event_id = snowflake.next_id();
        let note_text = match from {
            PartStatus::PENDING => format!(
                "送检：来自待下发 → 品检架 {}",
                target_shelf.code
            ),
            PartStatus::PROGRAMMING => format!(
                "送检：来自编程中 → 品检架 {}",
                target_shelf.code
            ),
            PartStatus::IN_PROCESS => format!(
                "送检：来自生产架 → 品检架 {}",
                target_shelf.code
            ),
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

    /// to_inspection 薄包装（handler 调用）。
    pub async fn to_inspection(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ToInspectionRequest,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        let target_inspection_shelf_id: i64 = req.target_inspection_shelf_id.parse().map_err(|_| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("target_inspection_shelf_id '{}' is not a numeric id", req.target_inspection_shelf_id),
            )
        })?;
        let batch_id = req.batch_id.as_deref().and_then(|s| s.parse().ok());
        Self::to_inspection_core(
            &mut *conn, snowflake, part_id, target_inspection_shelf_id,
            batch_id, req.quantity, req.note.as_deref(), current,
        ).await
    }

    /// 批量 to-inspection：每个 item 在 caller 的外层事务内执行（共享 `&mut PgConnection`）。
    ///
    /// 失败 item **不中断** 后续 item；失败原因收集到 `failed` Vec。
    ///
    /// 与 `batch_to_ship` 同形；额外校验：
    /// - `items.is_empty()` → 40001
    /// - `items.len() > BATCH_TO_INSPECTION_MAX_ITEMS` → 40001
    /// - `target_inspection_shelf_id` 在循环外一次性做（zone='INSPECTION' +
    ///   is_active=true）—— 不合法 → 顶层 20511 / 20512（**整批失败**，不让
    ///   per-item 循环开始）。
    pub async fn batch_to_inspection(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: BatchToInspectionRequest,
        current: &CurrentUser,
    ) -> Result<BatchToXxxOut, AppError> {
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        if req.items.len() > BATCH_TO_INSPECTION_MAX_ITEMS {
            return Err(AppError::validation(format!(
                "items 数量 {} 超过上限 {}",
                req.items.len(),
                BATCH_TO_INSPECTION_MAX_ITEMS
            )));
        }
        let target_inspection_shelf_id: i64 = req.target_inspection_shelf_id.parse().map_err(|_| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("target_inspection_shelf_id '{}' is not a numeric id", req.target_inspection_shelf_id),
            )
        })?;
        // 共享品检架校验（一次性）—— 不合法 → 顶层 20511 / 20512（整批失败）
        Self::_validate_inspection_shelf(&mut *conn, target_inspection_shelf_id).await?;

        let mut submitted: Vec<ToXxxOut> = Vec::new();
        let mut failed: Vec<BatchOpFailure> = Vec::new();
        for (idx, item) in req.items.iter().enumerate() {
            // 解析 batch_id：失败 → 推 sentinel failure + continue
            let parsed_bid: i64 = match item.batch_id.parse::<i64>() {
                Ok(n) => n,
                Err(_) => {
                    failed.push(BatchOpFailure {
                        batch_id: 0,
                        code: code::VALIDATION_ERROR,
                        message: format!(
                            "batch_id '{}' 解析失败",
                            item.batch_id
                        ),
                    });
                    continue;
                }
            };
            // 反查 batch（拿 part_id + 校验存在 + 校验未软删）
            let target = match Self::_lookup_batch_by_id(&mut *conn, parsed_bid).await {
                Ok(t) => t,
                Err(e) => {
                    failed.push(BatchOpFailure {
                        batch_id: parsed_bid,
                        code: e.code(),
                        message: format!("{e}"),
                    });
                    continue;
                }
            };
            // per-item savepoint：失败 item 回滚部分写入，不影响后续 item（参考 batch_create_parts）
            use sqlx::AssertSqlSafe;
            let sp_name = format!("batch_to_inspection_item_{idx}");
            sqlx::raw_sql(AssertSqlSafe(format!("SAVEPOINT {sp_name}")))
                .execute(&mut *conn)
                .await?;
            match Self::to_inspection_core(
                &mut *conn,
                snowflake,
                target.part_id,
                target_inspection_shelf_id,
                Some(target.id),
                item.quantity,
                None,
                current,
            )
            .await
            {
                Ok(o) => {
                    sqlx::raw_sql(AssertSqlSafe(format!("RELEASE SAVEPOINT {sp_name}")))
                        .execute(&mut *conn)
                        .await?;
                    submitted.push(o);
                }
                Err(e) => {
                    sqlx::raw_sql(AssertSqlSafe(format!("ROLLBACK TO SAVEPOINT {sp_name}")))
                        .execute(&mut *conn)
                        .await?;
                    failed.push(BatchOpFailure {
                        batch_id: target.id,
                        code: e.code(),
                        message: format!("{e}"),
                    });
                }
            }
        }
        // 父装配件同步已由各 to_inspection_core 在 savepoint 内调用 sync_from_part_change 完成。
        Ok(BatchToXxxOut { submitted, failed })
    }
}