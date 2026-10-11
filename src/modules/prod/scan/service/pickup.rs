//! prod::scan 的手动 pick-up：`POST /api/v2/prod/scan/batches/{batch_id}/pick-up`
//!
//! 起点 `PENDING` / `IN_PROCESS+PRODUCTION_SHELF` → 目标 `IN_PROCESS+WORKER`。
//! Manager / Clerk / ShelfAccount 三角色可触发；worker 必须 active 且绑定 work_type。
//!
//! 2026-10-10 自 `prod::batch::service::pickup` 搬来（硬切无 alias）：唯一消费方
//! 是报工台的取件页与队列看板，按「目标域按前端消费方判定」的规约归 `prod::scan`。
//! 业务逻辑逐字不变。
//!
//! 部分领取（`quantity` 缺省 = 整批）见 [`ScanService::pick_up`] 的「部分领取语义」段。
//!
//! ## 2026-10-15 S2-B：PENDING 起点的链首落位
//!
//! PENDING 批次此前是**透传** `current_process_id` / `current_process_step_id`
//! （PENDING 行两列恒 NULL）⇒ 领到手的新 IN_PROCESS 批次工序信息缺失，落不进任何
//! 工序候选池。现在 PENDING 分支在**容量守卫之后、拆批之前**解析链首
//! （[`resolve_pending_chain_head`]）：有链 ⇒ 落链首 step + process（链首 ∉ 工种
//! ⇒ `20104`，链不可解析 ⇒ `20702`）；无链 ⇒ 两列维持 NULL（已知口径）。
//! IN_PROCESS 分支与部分领取的余量去向**一行不动**。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part::vo::PartOut;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::modules::prod::queue::service::QueueService;
use crate::modules::prod::scan::dto::PickUpRequest;
use crate::shared::error::{AppError, code};
// 2026-10-12：`t_part_event` 的写侧自 part 域的事件 INSERT 上提到
// `shared::part_event`（该表由 7 个模块写、无域归属）；本文件的 2 处构造点同时
// 补齐 `worker_id` / `process_id` 两个新字段（见 `part_event/mod.rs` 写入点清单）。
use crate::shared::part_event::{NewPartEvent, insert};

use super::ScanService;
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

impl ScanService {
    /// `POST /scan/batches/{batch_id}/pick-up`：手动 pick-up。
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
    /// ⚠️ 与 [`crate::modules::prod::batch::service::batch_ops::BatchService::split_batch`]
    /// 的**数量语义差异**：
    /// 那边要求 `quantity < batch.quantity`（等于即非法），因为「等于」在 split
    /// 语境下是「白拆一次」；这边允许 `==` ，因为 `==` 就是**整批领取**的显式
    /// 写法，无需拆。故只有 `>` 才非法。
    ///
    /// OCC 仍锚**源批次**（`validate_batch_version(batch.id, req.version, …)` 在
    /// 最前、拆批 SQL 的 `src_version` 也传 `req.version`）；新批次由
    /// `_split_batch_inner` 建出来时 `version` 恒为 0，后续写入按 0 锚。
    ///
    /// ## 2026-10-15 S2-B：PENDING 起点的链首落位与两条新错误码
    ///
    /// PENDING 起点不再透传 NULL 工序：有链时落**链首**
    /// （[`resolve_pending_chain_head`]，位置在容量守卫之后、拆批之前）——
    /// - `20702 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`：链行已软删 / 链内无活跃 step；
    /// - `20104 BIZ_INVALID_VALUE`：链首 ∉ 该工种的活跃工序映射。
    ///
    /// 无链时两列仍为 NULL（已知口径，与改动前逐字一致）；IN_PROCESS 起点一行不动。
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
        // batch_id 来自 URL 路径参数（`POST /scan/batches/{batch_id}/…`），
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
        // 2026-10-14 新增：容量守卫（修「报工台取件无上限」bug）。
        //
        // 本端点此前**完全没有** `max_held_batches` 守卫 —— 工人在队列板上被
        // `held < max` 卡着，从报工台取件页却可以无限往手上加，`move_batch` /
        // `take_one_from_pool` 两条路径的容量契约在这里形同虚设。
        //
        // 守卫调 `QueueService::ensure_worker_has_capacity`（queue 域的**单一来源**，
        // 与 `move_batch` 两个分支同一处），判据 = `docs/api/queue.md` §4.3 的
        // 「工人持有」，超限返 `BIZ_WORKER_HOLD_LIMIT_EXCEEDED`；工种未配上限返
        // `BIZ_WORK_TYPE_MAX_HELD_NOT_SET`（NULL 时该工种**不可取件**）。
        //
        // ⚠️ **本守卫只按 `max_held_batches`（硬上限）判，与 `auto_allocate_limit`
        // （自动补料上限，2026-10-15 新增）无关**：pick-up 是**手动单批路径**，
        // 自动上限只约束 refill / auto-allocate / worker-scan 同事务 refill 三条
        // 自动路径。语义上「自动补料只补到 auto、人工仍可手动领到 max」是刻意的
        // 分工，不要为本端点补 auto 校验。
        //
        // ⚠️ **位置是硬约束**，不要挪动（两条边界 + 一条次序理由）：
        // - 必须在上面 worker `is_active` / `work_type_id` 校验**之后** —— 本守卫
        //   只收 `work_type_id`，重复校验 worker 是噪声；`wt_id` 的 `is_none()`
        //   分支已提前 return，故此处恒为 `Some`；
        // - 必须在 `split_batch_for_partial_pass` **之前** —— 否则「部分领取」会
        //   先把批次拆出一个新批次、随即撞容量守卫失败，留下一个没人认领的孤儿
        //   新批次（源批次数量已被扣减）。**这条是本守卫存在价值的一半**：光校验
        //   不够，必须保证失败发生在任何写动作之前；
        // - **刻意排在 `validate_shelf_zone` 之后**：守卫是本轮**新增**的检查，
        //   不得插队到既有检查之前改变错误码优先级 —— 「非法 shelf + 工种未配
        //   上限」这种组合在改动前只会返回货架错误（20501 / 20512 / 20104），
        //   插队会让它变成 20904，属于本轮范围外的破坏性变更。
        // - 仍在事务内（handler 的 `state.pool.begin()`），守卫失败时 tx drop
        //   隐式回滚。
        //
        // ⚠️ 跨域 import 是允许的：`prod::scan` 是**转发型域**，整域不适用域隔离
        //   护栏（只有 `listing/` 子模块适用，且那条护栏扫不到本文件）。
        //   同款依赖先例：`prod::scan::handler::transition` 已 import
        //   `QueueService::refill_for_worker_with_work_type`。
        //
        // ⚠️ 返回值 `(current_held, max_held)` 是给 `QueueService::move_batch`
        //   填 `MoveResult` 展示字段用的，本端点不关心 ⇒ 显式 `let _ =` 丢弃。
        //   **不要**写成 `.await?;` 然后指望能用到它 —— 那是笔误，会被
        //   clippy 的 `let_underscore_must_use` 之外的手段漏过去；`let _ =` 在
        //   本仓是「刻意丢弃」的既有写法。
        let work_type_id = wt_id.expect("wt_id 已由上方 is_none 分支保证非空");
        let _ =
            QueueService::ensure_worker_has_capacity(repo.conn_mut(), req.worker_id, work_type_id)
                .await?;
        // 2026-10-15 S2-B 新增：PENDING 起点的**链首落位**（见 [`resolve_pending_chain_head`]）。
        //
        // ⚠️ 位置是硬约束（两条边界 + 一条次序理由）：
        // - 必须在上面容量守卫**之后** —— 这是本轮**新增**的检查，不得插队到既有
        //   检查之前改变错误码优先级（「PENDING + 链不可解析 + 工人已满」在改动前
        //   返回容量错误，插队会把它变成 20702，属本轮范围外的破坏性变更）；
        // - 必须在 `split_batch_for_partial_pass` **之前** —— 否则「部分领取」会先
        //   拆出一个新批次、随即撞链校验失败，留下一个没人认领的孤儿新批次（源批次
        //   数量已被扣减）。与容量守卫同款的存在价值；
        // - IN_PROCESS 起点**不解析**：翻状态不写这两列（见下方 inline SQL 的注释），
        //   事件取源批次现值 ⇒ 行为与改动前逐字一致。
        //
        // 判据与读侧 `locate` 的 PENDING 闸门同源（`has_usable_chain_head` /
        // `chain_head_in_work_type` 两个投影与 [`resolve_pending_chain_head`] 逐谓词
        // 对齐），避免「读侧给 PICK_UP、写侧必拒」的分叉。
        let (placed_step_id, placed_process_id): (Option<i64>, Option<i64>) =
            if from == PartStatus::PENDING {
                resolve_pending_chain_head(repo.conn_mut(), part_id, work_type_id).await?
            } else {
                (batch.current_process_step_id, batch.current_process_id)
            };
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
        //
        // 2026-10-15 S2-B：PENDING 分支的两列改用 [`resolve_pending_chain_head`] 的
        // 解析值（有链 = 链首 step / process，无链 = 两列 NULL）；IN_PROCESS 分支
        // 依旧一行不动（只翻 location+holder，status 与工序列保持原值 —— 池内领取
        // 「不推进工序」的既有语义）。
        let n = if from == PartStatus::PENDING {
            mark_batch_with_status_and_meta(
                repo.conn_mut(),
                target_batch_id,
                target_version,
                "IN_PROCESS",
                Some("WORKER"),
                Some(req.worker_id),
                placed_step_id,
                placed_process_id,
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
            insert(
                repo.conn_mut(),
                NewPartEvent {
                    id: snowflake.next_id(),
                    part_id,
                    event_type: "SPLIT",
                    from_status: Some(&batch.status),
                    to_status: Some(&batch.status),
                    batch_id: Some(info.new_batch_id),
                    quantity: Some(info.quantity),
                    drawing_code: Some(&part.drawing_no),
                    badge_code: None,
                    // 2026-10-12：拆批事件挂在**新批次**上，工序取 `placed_process_id`。
                    // 2026-10-15 S2-B：`placed_process_id` 对 PENDING 起点是**链首**
                    // （新批次随后在翻状态里落位到它，事件快照与批次终态一致）；
                    // 对 IN_PROCESS 起点是源批次现值（拆批继承，两者同值）。无链的
                    // PENDING 仍为 None（与改动前逐字一致）。工人是 `req.worker_id`
                    // （已在上面查库校验存在且 active）。
                    worker_id: Some(req.worker_id),
                    process_id: placed_process_id,
                    note: Some("pick-up 部分领取自动拆批"),
                    created_by: Some(current.id),
                },
            )
            .await?;
        }
        // 2026-10-03：事件指向实际被领走的那一批、数量取实际交付量
        // （整批路径 = batch.quantity，与改动前一致）。
        let picked_qty = split_qty.unwrap_or(batch.quantity);
        insert(
            repo.conn_mut(),
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "PICKED_UP",
                from_status: Some(from.as_str()),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(target_batch_id),
                quantity: Some(picked_qty),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                // 2026-10-12：同上一处 —— `target_batch_id` 拆批时是新批次、整批时是
                // 源批次，`placed_process_id` 对两种起点都是「批次落位后的工序」
                // （PENDING = 链首 / NULL，IN_PROCESS = 源批次现值）。
                worker_id: Some(req.worker_id),
                process_id: placed_process_id,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
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

/// PENDING 起点领取时的**链首落位**（2026-10-15 S2-B 新增）。
///
/// 返回 `(step_id, process_id)`：
/// - **无链**（`t_part.process_chain_id IS NULL`，手写工单常态）→ `(None, None)`：
///   两列维持 NULL（登记为已知口径，与改动前逐字一致）；
/// - **有链** → 链内第一个未软删 step（`ProcessChainRepo::first_step_in_chain`，
///   链行软删闸门在它内部）：
///   - 取不到（链行已软删 / 链内无活跃 step）→ `20702 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`；
///   - 链首工序 ∉ 该工种的活跃工序映射 → `20104 BIZ_INVALID_VALUE`（判据与
///     `queue/move` POOL→WORKER 分支的 `WorkTypeRepo::list_process_ids` 同源）。
///
/// ## 位置硬约束（caller 侧）
/// 必须在**容量守卫之后、`split_batch_for_partial_pass` 之前**调用，理由见
/// [`ScanService::pick_up`] 内的调用点注释（不改变既有错误码优先级 + 失败不得
/// 留下拆批孤儿）。
///
/// ## 与读侧 `locate` 的同源关系
/// `locate` 的 PENDING 闸门读 `LocateBatchRow.has_usable_chain_head`（= 本节
/// 「链行未软删且链内有活跃 step」）与 `chain_head_in_work_type`（= 本节的映射
/// 校验），三者逐谓词对齐 —— 读侧给 `PICK_UP` 的形状写侧不会拒（残余失败组合见
/// `docs/api/scan.md` §8）。
async fn resolve_pending_chain_head(
    conn: &mut sqlx::PgConnection,
    part_id: i64,
    work_type_id: i64,
) -> Result<(Option<i64>, Option<i64>), AppError> {
    let Some(chain_id) = crate::shared::batch::optional_process_chain(conn, part_id).await? else {
        // 无链：两列维持 NULL（PENDING 行本来就是 NULL，随后 mark_* 传 None 即
        // 「写 NULL」，与改动前逐字一致）。
        return Ok((None, None));
    };
    let (step_id, process_id) =
        crate::modules::prod::process_chain::repo::ProcessChainRepo::first_step_in_chain(
            &mut *conn, chain_id,
        )
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                format!(
                    "chain {chain_id} 不可解析（已软删）或链内没有活跃 step，PENDING 领取无处落位"
                ),
            )
        })?;
    let process_ids = crate::modules::prod::work_type::repo::WorkTypeRepo::list_process_ids(
        &mut *conn,
        work_type_id,
    )
    .await?;
    if !process_ids.contains(&process_id) {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("工种 {work_type_id} 不含链首工序 {process_id}（PENDING 领取落位）"),
        ));
    }
    Ok((Some(step_id), Some(process_id)))
}
