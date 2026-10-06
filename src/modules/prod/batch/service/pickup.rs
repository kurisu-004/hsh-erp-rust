//! prod::batch 的手动 pick-up：`POST /api/v2/prod/batches/{batch_id}/pick-up`
//!
//! 起点 `PENDING` / `IN_PROCESS+PRODUCTION_SHELF` → 目标 `IN_PROCESS+WORKER`。
//! Manager / Clerk / ShelfAccount 三角色可触发；worker 必须 active 且绑定 work_type。
//!
//! 2026-10-03 新增：部分领取（`quantity` 缺省 = 整批，行为与此前逐字一致）。
//! 见 [`BatchService::pick_up`] 的「部分领取语义」段。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part::vo::PartOut;
use crate::modules::prod::batch::dto::PickUpRequest;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::shared::error::{AppError, code};

use super::BatchService;
use crate::shared::batch::guards::{
    mark_batch_with_status_and_meta, validate_batch_version, validate_shelf_zone,
};

/// 部分领取自动拆批的附加信息（整批路径为 `None`）。
///
/// 2026-10-03 新增。handler 拿它在 commit 之后补发 `PART_BATCH_SPLIT` 广播。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickUpSplitInfo {
    /// 拆出来、被工人领走的那一批的雪花 id。
    pub new_batch_id: i64,
    /// 拆给工人的数量（= 新批次 quantity）。
    pub quantity: i32,
    /// 所属工单（供 WS payload 复用，避免 handler 再查一次）。
    pub part_id: i64,
}

/// pick-up 的结果：part 级视图 + 「实际被领走的是哪一批、多少件」。
///
/// 2026-10-03 新增返回结构（此前是裸 `PartOut`）。**HTTP 响应体形状不变**
/// （仍是 `R<PartOut>`），新增的字段只服务 handler 的 WS 广播：
/// `PART_PICKED_UP` 要带 `batch_id` + `quantity`，而「领走的是源批次还是拆出来
/// 的新批次」只有 service 内知道。
#[derive(Debug, Clone)]
pub struct PickUpOutcome {
    /// part 级视图（响应体 `data`）。
    pub part: PartOut,
    /// 实际被领走的批次：拆批时 = 新批次，整批时 = URL 里的源批次。
    pub picked_batch_id: i64,
    /// 实际被领走的数量。
    pub picked_quantity: i32,
    /// 部分领取自动拆批信息；整批路径为 `None`。
    pub split: Option<PickUpSplitInfo>,
}

impl BatchService {
    /// `POST /prod/batches/{batch_id}/pick-up`：手动 pick-up。
    /// 起点：PENDING / IN_PROCESS+PRODUCTION_SHELF → 目标 IN_PROCESS+WORKER。
    ///
    /// 不变量：
    /// - worker 必须 is_active 且 work_type_id 不为 NULL
    /// - shelf（**仅当调用方传了 `shelf_id`**，2026-10-04 起可选）必须 zone=PRODUCTION
    ///   且 active；缺省则完全不校验
    /// - 状态机迁移：`{PENDING, IN_PROCESS} → IN_PROCESS`（DB 状态相同；service 守 location）
    /// - 写 PICKED_UP 事件 + 广播 PART_PICKED_UP WS
    ///
    /// ## 部分领取语义（2026-10-03 新增）
    ///
    /// `req.quantity` 三种落法：
    /// - `None` / `>= batch.quantity` → **整批路径**，行为与本次改动前逐字一致
    ///   （这是回归基线，不允许有任何差异）；
    /// - `0 < quantity < batch.quantity` → **部分路径**：先
    ///   `split_batch_for_partial_pass` 拆出新批次，再把**新批次**翻到
    ///   IN_PROCESS+WORKER，源批次原地保留、数量递减；
    /// - `<= 0` / `> batch.quantity` → `BIZ_PART_BATCH_INVALID_QUANTITY`。
    ///
    /// ⚠️ 与 [`super::batch_ops::BatchService::split_batch`] 的**数量语义差异**：
    /// 那边要求 `quantity < batch.quantity`（等于即非法），因为「等于」在 split
    /// 语境下是「白拆一次」；这边允许 `==` ，因为 `==` 就是**整批领取**的显式
    /// 写法，无需拆。故只有 `>` 才非法。
    ///
    /// OCC 仍锚**源批次**（`validate_batch_version(batch.id, req.version, …)` 在
    /// 最前、拆批 SQL 的 `src_version` 也传 `req.version`）；新批次由
    /// `_split_batch_inner` 建出来时 `version` 恒为 0，后续写入按 0 锚。
    pub async fn pick_up<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: PickUpRequest,
        current: &CurrentUser,
    ) -> Result<PickUpOutcome, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::ShelfAccount])?;
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
        if from != PartStatus::PENDING && from != PartStatus::IN_PROCESS {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("pick-up 起点 {from:?} 不允许"),
            ));
        }
        if from == PartStatus::IN_PROCESS && batch.location.as_deref() != Some("PRODUCTION_SHELF") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "pick-up: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上",
            ));
        }
        // 2026-10-03 新增：数量校验 + 是否需要拆批。
        //
        // ⚠️ 与 `batch_ops::split_batch` 的语义差异见本函数 doc：`quantity ==
        // batch.quantity` 在这里是合法的「显式整批」，只有 `>` 才非法。
        let requested: Option<i32> = match req.quantity {
            None => None,
            Some(raw) => {
                let qty: i32 = raw.try_into().map_err(|_| {
                    AppError::biz(
                        code::BIZ_PART_BATCH_INVALID_QUANTITY,
                        "quantity 超出 i32 范围",
                    )
                })?;
                if qty <= 0 {
                    return Err(AppError::biz(
                        code::BIZ_PART_BATCH_INVALID_QUANTITY,
                        format!("quantity {qty} 必须 > 0"),
                    ));
                }
                if qty > batch.quantity {
                    return Err(AppError::biz(
                        code::BIZ_PART_BATCH_INVALID_QUANTITY,
                        format!("quantity {qty} 超过 batch.quantity {}", batch.quantity),
                    ));
                }
                Some(qty)
            }
        };
        // worker 必须 active + 有 work_type
        let worker: Option<(bool, Option<i64>)> = sqlx::query_as(
            "SELECT is_active, work_type_id FROM t_worker \
                 WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(req.worker_id)
        .fetch_optional(repo.conn_mut())
        .await?;
        let (is_active, wt_id) = worker.ok_or_else(|| {
            AppError::biz(
                code::BIZ_WORKER_NOT_FOUND,
                format!("worker {} 不存在", req.worker_id),
            )
        })?;
        if !is_active {
            return Err(AppError::biz(
                code::BIZ_WORKER_INACTIVE,
                format!("worker {} 已停用", req.worker_id),
            ));
        }
        if wt_id.is_none() {
            return Err(AppError::biz(
                code::BIZ_WORKER_NO_WORK_TYPE,
                format!("worker {} 未绑定 work_type", req.worker_id),
            ));
        }
        // 2026-10-04 新增：`shelf_id` 改为可选，**缺省 = 完全不校验**。
        //
        // 该字段对 pick-up 的最终结果零影响（本路径 3 个 `t_part_batch` 写入点的
        // SET / WHERE 均无货架列或货架条件，`t_part_event` 无货架列，响应 VO 无
        // shelf 字段），原先那条 `validate_shelf_zone` 是防呆断言而非安全边界，
        // 故扫码台 / 看板等自动发起方可以不带它。传了才校验，语义与改动前逐字
        // 一致（20501 / 20512 / 20104 三条错误码不变）。
        //
        // ⚠️ 位置不动：仍在事务内、仍在 worker 校验之后、仍在拆批之前 —— 保证
        // 「非法 shelf 不拆批」这条既有性质不变。
        if let Some(shelf_id) = req.shelf_id {
            validate_shelf_zone(repo.conn_mut(), shelf_id, "PRODUCTION").await?;
        }
        // 2026-10-03 新增：部分领取 —— 翻状态**之前**先把要交出去的那部分拆成
        // 新批次；之后所有针对批次的写入都改指新批次（version 恒为 0）。
        //
        // 走 `split_batch_for_partial_pass`（`INSERT ... SELECT` 继承 location /
        // current_holder_id / current_process_id / current_process_step_id /
        // is_repairing）而不是 `PartBatchRepo::split_batch`：pick-up 的起点状态
        // 必须在后续的翻状态校验里**同样成立**（IN_PROCESS 起点要求批次停在
        // PRODUCTION_SHELF 上），拆出来的新批次得带着这个 location 出发。
        //
        // `new_batch_status` 传源批次 status：新批次与源批次同状态，后续 pick-up
        // 的状态守卫（PENDING / IN_PROCESS）对它等价成立。
        let split_qty: Option<i32> = requested.filter(|q| *q < batch.quantity);
        let mut split: Option<PickUpSplitInfo> = None;
        let (target_batch_id, target_version) = match split_qty {
            Some(qty) => {
                let new_id = PartBatchRepo::split_batch_for_partial_pass(
                    repo.conn_mut(),
                    snowflake.next_id(),
                    batch.id,
                    req.version,
                    part_id,
                    qty,
                    &batch.status,
                    Some(current.id),
                )
                .await
                .map_err(|e| match e {
                    // 源批次 version 已变 / 已软删（与 split_batch 同款翻译）
                    sqlx::Error::RowNotFound => {
                        AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突")
                    }
                    other => AppError::from(other),
                })?;
                split = Some(PickUpSplitInfo {
                    new_batch_id: new_id,
                    quantity: qty,
                    part_id,
                });
                // 新批次由 `_split_batch_inner` 建出时 version 恒为 0
                (new_id, 0i32)
            }
            None => (batch.id, req.version),
        };
        // 翻状态：PENDING → IN_PROCESS+WORKER；IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER
        // PR-3：保留 batch.current_process_step_id（pick-up 不改 step，只换 holder）
        // 2026-09-30：同理透传 batch.current_process_id（池归属权威依据；
        // pick-up 不改工序，只换 holder）
        let n = if from == PartStatus::PENDING {
            mark_batch_with_status_and_meta(
                repo.conn_mut(),
                target_batch_id,
                target_version,
                "IN_PROCESS",
                Some("WORKER"),
                Some(req.worker_id),
                batch.current_process_step_id,
                batch.current_process_id,
                current.id,
            )
            .await?
        } else {
            // IN_PROCESS：只翻 location+holder，status 保持 IN_PROCESS
            // PR-3：删 placed_at 写入（列已删）
            sqlx::query(
                "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = $3, \
                     version = version + 1, updated_at = now(), updated_by = $4 \
                     WHERE id = $1 AND version = $2 AND status = 'IN_PROCESS' \
                       AND deleted_at IS NULL",
            )
            .bind(target_batch_id)
            .bind(target_version)
            .bind(req.worker_id)
            .bind(current.id)
            .execute(repo.conn_mut())
            .await?
            .rows_affected()
        };
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // 2026-10-03 新增：拆批留痕。源批次数量被静默扣减，若只留 PICKED_UP
        // 事件，事后只看事件流就看不出「原来 10 件、为什么源批次只剩 6 件」。
        // 事件挂在 part 维度、batch_id 指向新批次，与 `batch_ops::split_batch`
        // 的 SPLIT 事件同形。
        if let Some(info) = split.as_ref() {
            repo.insert_part_event(NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "SPLIT",
                from_status: Some(&batch.status),
                to_status: Some(&batch.status),
                batch_id: Some(info.new_batch_id),
                quantity: Some(info.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: Some("pick-up 部分领取自动拆批"),
                created_by: Some(current.id),
            })
            .await?;
        }
        // 2026-10-03：事件指向实际被领走的那一批、数量取实际交付量
        // （整批路径 = batch.quantity，与改动前一致）。
        let picked_qty = split_qty.unwrap_or(batch.quantity);
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "PICKED_UP",
            from_status: Some(from.as_str()),
            to_status: Some("IN_PROCESS"),
            batch_id: Some(target_batch_id),
            quantity: Some(picked_qty),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.note.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "pick-up 后查不到"))?;
        Ok(PickUpOutcome {
            part: PartOut::from(fresh),
            picked_batch_id: target_batch_id,
            picked_quantity: picked_qty,
            split,
        })
    }
}
