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
use crate::shared::error::{code, AppError};

use super::dto::{
    BatchPassFailure, BatchPassInspectionOut, BatchPassInspectionRequest, PartOut,
};

/// 批量端点单次请求最大 item 数（handler/service 双层校验）。
pub const BATCH_PASS_INSPECTION_MAX_ITEMS: usize = 200;

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
        _snowflake: &SnowflakeIdGenerator,
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

        // 多轮 rollup 守卫：仅当无其它 INSPECTION 批次时才翻 `t_part.status`。
        // 否则其它批次仍在 INSPECTION 状态，工单必须保留 INSPECTION
        // （对齐 Python `_rollup_part_status` 的 `least(batches.status)` 语义）。
        let other_inprocess =
            PartRepo::count_other_inprocess_batches(&mut *conn, part_id).await?;
        if other_inprocess > 0 {
            // 还有别的 INSPECTION 批次存在，跳过 part.status 翻转；
            // 批次状态由 `t_part_batch` 自身记录（含 updated_by），
            // 工单保留 INSPECTION 即可。返回读到的旧 `PartOut`。
            let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
                .await?
                .ok_or_else(|| {
                    AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished"))
                })?;
            return Ok(PartOut::from(fresh));
        }

        // 6. UPDATE t_part: 同步工单状态（OCC + 写 updated_by）。这一步解决
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

        // 7. 写 t_part_event 事件日志。事件 id 走雪花生成（App 侧生成，DB 默认列对齐）。
        let event_id = _snowflake.next_id();
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
            let bid = item.batch_id.as_deref().and_then(|s| s.parse().ok());
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
}