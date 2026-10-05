//! prod::batch 的 to-XXX 三个共享核心：`to_ship_core` / `to_process_core` /
//! `to_inspection_core`。
//!
//! 与 `transition.rs` 的薄 wrapper / 批量聚合器同属 `impl BatchService`，分文件
//! 只为满足 docs/conventions.md 的单文件行数上限。
//!
//! ## 签名约定
//! `<R: PartRepoTrait>(repo: &mut R, ...)`：事务边界由 caller（handler / 批量
//! 聚合器）保证；本方法只跑业务校验 + repo 调用 + 状态机守卫。生产
//! `R = &mut PgConnection`，跨域 repo 与 inline sqlx 走 `repo.conn_mut()`。

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::service::SyncOutcome;
use crate::modules::part::model::{NewPartEvent, TPartInspected};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part::vo::PartOut;
use crate::modules::prod::batch::model::TPartBatch;
use crate::modules::prod::batch::vo::ToXxxOut;
use crate::shared::error::{AppError, code};

use super::BatchService;
use super::guard::{optional_process_chain, optional_step_id};

impl BatchService {
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
    pub async fn to_ship_core<R: PartRepoTrait>(
        repo: &mut R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        expected_batch_version: i32,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        // 0. 反查批次 → part_id（2026-10-02 去 part 化：batch_id 是 URL 路径参数，
        //    批次 id 全局唯一即锚点，part_id 只能由批次行反查得到）。
        let anchor = Self::_lookup_batch_by_id(repo, batch_id).await?;
        let part_id = anchor.part_id;
        // 1. 读 part
        let part: TPartInspected = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
        })?;

        // 2. 状态机守卫：INSPECTION → READY_TO_SHIP
        //
        //    2026-10-06 读 `anchor.status`（批次真源）而非 `part.status`（派生缓存列）。
        //    `t_part.status` 由 `rollup_part_derived` 按 min-progress 派生：同工单只要
        //    还有任一批次进度更靠前，整单就派生成那个更早的状态。分批生产的工单因此
        //    会出现「批次本身 INSPECTION、`t_part.status` 却是 IN_PROCESS / PENDING /
        //    OUTSOURCE」的形态，用派生列判批次流转会**成片假阴性**（本守卫的假阴性
        //    覆盖面是 5 种可能派生值里的 4 种）。端点自 2026-10-02 起以 `batch_id` 为
        //    锚点（URL 从 `/parts/{part_id}/to-ship` 硬切到
        //    `/prod/batches/{batch_id}/to-ship`），判据却没跟着换，直到本次才修。
        //    房内已正确实现的同类端点见 `shelf.rs::place_on_shelf` /
        //    `outsource.rs::send_to_outsource` / `lifecycle.rs::deliver`（同款注释）。
        let from = PartStatus::from_str(&anchor.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("batch {batch_id} 状态非法: {}", anchor.status),
            )
        })?;
        if !from.can_transition_to(PartStatus::READY_TO_SHIP) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "batch {batch_id} 当前状态 {} 不允许通过品检（必须先送检）",
                    from.as_str()
                ),
            ));
        }

        // 3. 校验锚定批次处于 INSPECTION 状态。
        //
        //    2026-10-02：`batch_id` 是必填路径参数，SQL 走 `Some` 分支
        //    （`WHERE id = $1 AND status = 'INSPECTION'`，`AND part_id = $2` 恒真已删）。
        //    该分支是 `fetch_optional` ⇒ 只回 `Ok(None)` 或真 DB 错，不会回
        //    `RowNotFound`，故「多批次歧义」这一形态在此**不存在**（批次 id 全局
        //    唯一即锚点）。repo 的 `None` 分支（按 part 消歧）仍被 `scan.rs` 的
        //    「当前 INSPECTION 批次」查询使用。
        let target: TPartBatch = match repo
            .find_inprocess_batch_for_part(part_id, Some(batch_id))
            .await
        {
            Ok(Some(b)) => b,
            Ok(None) => {
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("batch {batch_id} 不是 INSPECTION 状态的批次"),
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
            Self::_split_for_partial_op(repo, snowflake, &target, quantity, current).await?;

        // 5. UPDATE t_part_batch: INSPECTION → READY_TO_SHIP（OCC + 写 updated_by）
        //
        // 2026-10-01：写与派生已焊在 `crate::modules::prod::batch::status_gate` 的
        // `apply_batch_status_change` 一个函数里，故这里拿到的 `rollup` **就是**
        // batch → part → assembly 的最终结果：0 行由 gate 直接抛 40901
        // `VERSION_CONFLICT`（包装函数恒返回 1，caller 不需要、也不应该自己判断
        // 影响行数）；`rollup.sync` 供下方第 7 步填 `synced_assembly_id`。
        let rollup = repo
            .mark_batch_passed_inspection(operated_id, operated_version, Some(current.id))
            .await?;

        // 6. 写 t_part_event 事件日志（无条件）
        let event_id = snowflake.next_id();
        repo.insert_part_event(NewPartEvent {
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
        })
        .await?;

        // 7. batch → part → assembly rollup：**已在第 5 步的 status_gate 内完成**
        //    （min-progress 规则：多条 INSPECTION 批次时 part 维持 INSPECTION，
        //    单条时升 READY_TO_SHIP；part.status 真变了才级联
        //    `AssemblyService::sync_from_part_change`）。
        //    2026-10-01：此处**不再**补调 `PartService::sync_from_batch_change`
        //    —— 那会跑第二次派生、必然 `NoChange`，把 `synced_assembly_id`
        //    恒吞成 null（连带 WS 的 `ASSEMBLY_UPDATED` 永不发）。
        let synced = rollup.sync;
        let synced_assembly_id = match synced {
            SyncOutcome::Changed(aid) => Some(aid),
            SyncOutcome::NoChange => None,
        };

        // 8. 重读返回
        let fresh = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
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
    /// - 20118 `BIZ_PART_REPAIR_NOT_TRIGGERED` —— 目标批次**处于返修中**
    ///   （`is_repairing = true`），应改用 `complete-repair`（step 4.6 的守卫）
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20512 `BIZ_SHELF_INACTIVE`
    /// - 40901 `VERSION_CONFLICT`
    #[allow(clippy::too_many_arguments)]
    pub async fn to_process_core<R: PartRepoTrait>(
        repo: &mut R,
        snowflake: &SnowflakeIdGenerator,
        shelf_id: i64,
        next_process_id: i64,
        note: Option<&str>,
        batch_id: i64,
        expected_batch_version: i32,
        quantity: Option<i32>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        // 0. 反查批次 → part_id（2026-10-02 去 part 化，理由同 `to_ship_core`）
        let anchor = Self::_lookup_batch_by_id(repo, batch_id).await?;
        let part_id = anchor.part_id;
        // 1. 读 part
        let part: TPartInspected = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
        })?;
        // 2. 状态机守卫：必须 INSPECTION
        //
        //    2026-10-06 读 `anchor.status`（批次真源）而非 `part.status`（派生缓存列），
        //    理由与假阴性覆盖面见 `to_ship_core` 同处注释。本端点的假阴性窗口比
        //    to-ship 窄（`t_part.status` 只可能派生到 IN_PROCESS 这一个被误拒的值），
        //    但成因完全相同，一并修正。
        let from = PartStatus::from_str(&anchor.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("batch {batch_id} 状态非法: {}", anchor.status),
            )
        })?;
        if !from.can_transition_to(PartStatus::IN_PROCESS) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "batch {batch_id} 当前状态 {} 不允许品检打回（必须先送检到 INSPECTION）",
                    from.as_str()
                ),
            ));
        }
        // 3. 校验 shelf（PRODUCTION 区 + active）
        Self::_validate_production_shelf_and_process(repo, shelf_id, next_process_id).await?;
        // 4. 校验锚定批次处于 INSPECTION 状态（2026-10-02：改为纯按 id 定位，
        //    `part_id` 形参与 `AND part_id = $2` 断言一并删除）
        let target: TPartBatch = match repo.find_inspection_batch_by_id(batch_id).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("batch {batch_id} 不是 INSPECTION 状态的批次"),
                ));
            }
            Err(e) => return Err(AppError::from(e)),
        };
        // 4.5 caller 侧乐观锁：锚定 batch 而非 part
        Self::_assert_batch_version(&target, expected_batch_version)?;
        // 4.6 返修守卫（2026-10-01 review 第 2 轮 MAJOR-3）
        //
        // **为什么 to-process 不能碰返修中的批次**：`is_repairing = true` 的批次
        // 走的是**返修闭环**（`scan-inspect(pass=false)` / `start-repair` 起修 →
        // `complete-repair` 落回生产架 / 送检架并清标记，见同目录 `scan.rs` 的
        // `BatchService::scan_inspect` 与 `repair.rs` 的
        // `BatchService::complete_repair`），它与本端点表达的是**两件不同的
        // 事**：
        // - to-process = 「检验不合格，回**正常生产流**继续做」；
        // - complete-repair = 「返修完成 / 到位」，是**唯一**被授权清 `is_repairing`
        //   的动作（`mark_batch_with_status_and_meta` 的 `is_repairing: Some(false)`
        //   漏斗，见 `service::guard::mark_batch_with_status_and_meta` 的
        //   「本漏斗 = 批次离开返修态的清零点」）。
        //
        // 若放行，`mark_batch_failed_inspection`（`is_repairing: None` = 保持）会把
        // 批次落到生产架却仍挂着 `is_repairing = true`：它在
        // `GET /prod/batches/repairing` 里**长期显示「返修中」**（实际是普通在制品），
        // `complete-repair` 还会继续接受它 → 用户可把在制品当「完成返修」搬走。
        // 这是**标记列化新引入**的洞：改造前返修批次 `status='REPAIRING'`，而
        // `allowed_from = ["INSPECTION"]` 根本不让它进入本端点。
        //
        // 可达链（2026-10-01 复核，reviewer 给的 scan-inspect 链**不成立**）：
        // `scan-inspect(pass=false)` 的第二步把批次写成 `status='IN_PROCESS'` +
        // `is_repairing=true`（`phase1/scan.rs` 的 else 分支），而本端点的
        // `find_inspection_batch_by_id` 只捞 `status='INSPECTION'`，故它捞不到；
        // 真正的可达链是「起修后送检」：
        // `start-repair`（`status=IN_PROCESS` + `is_repairing=true`，loc 可为
        // PRODUCTION_SHELF）→ `POST /prod/batches/{batch_id}/to-inspection`
        // （`mark_batch_inspected` 的 `is_repairing: None` = **保持**）→ 批次变
        // `INSPECTION` + `is_repairing=true` → 本端点捞得到它。
        // （worker-scan INSPECTED 同样只保持标记，是第二条同类入口。）
        //
        // 为什么不改成「清标记放行」：业务上 `to-process` 在这条链上的含义恰恰是
        // 「返修件的返修后重投」——检验不合格说明**返修没通过**，货还需要继续返修，
        // 清掉标记等于把「还没修好」静默改写成「正常在制品」。正确动作是让用户
        // 改调 `complete-repair`（它做的是同一件事：落生产架 + 写目标工序 +
        // 清标记），错误码沿用 20118（与「重复起修」同族的「返修流转前置条件
        // 不满足」），文案写明改调哪个端点。
        if target.is_repairing {
            return Err(AppError::biz(
                code::BIZ_PART_REPAIR_NOT_TRIGGERED,
                format!(
                    "to-process: batch {} 处于返修中（is_repairing = true），\
                     请改用 complete-repair 落回生产架（它同时清返修标记）",
                    target.id
                ),
            ));
        }
        // 5. 部分通过拆批
        let (operated_id, operated_version, new_batch_id_out) =
            Self::_split_for_partial_op(repo, snowflake, &target, quantity, current).await?;
        // 6. UPDATE t_part_batch: INSPECTION → IN_PROCESS + location/holder/process
        // PR-3 批次 step 化：解析 step_id（chain 内 process_id → step_id）。
        // 2026-10-03：内联读链改为 `optional_process_chain`（无链放行，step 落
        // NULL）—— 本调用点此前不查 `BIZ_PROCESS_CHAIN_STEP_NOT_FOUND` 以外的
        // 链错误，与其余 6 个端点收口到同一对守卫。
        let chain_id = optional_process_chain(repo.conn_mut(), part_id).await?;
        let step_id = optional_step_id(repo.conn_mut(), chain_id, next_process_id).await?;
        // 2026-10-01：写与派生已焊在 status_gate 内（0 行由 gate 抛 40901，
        // 原 `if n == 0` 是死代码）；`rollup.sync` 供第 8 步填
        // `synced_assembly_id`。
        let rollup = repo
            .mark_batch_failed_inspection(
                operated_id,
                operated_version,
                shelf_id,
                // 2026-10-03：无链时 None ⇒ 写 NULL（与 process 列同一约定）
                step_id,
                // 2026-09-30：检验不合格打回生产架 = 进池 → 写目标工序
                Some(next_process_id),
                Some(current.id),
            )
            .await?;
        // 7. 写事件日志
        let event_id = snowflake.next_id();
        repo.insert_part_event(NewPartEvent {
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
        })
        .await?;
        // 8. batch → part → assembly rollup：**已在上面那次
        //    `mark_batch_failed_inspection`（status_gate）内完成**，不再补调
        //    `PartService::sync_from_batch_change`（2026-10-01：第二次派生必然
        //    `NoChange`，会把 `synced_assembly_id` 恒吞成 null）。
        let synced_assembly_id = match rollup.sync {
            SyncOutcome::Changed(aid) => Some(aid),
            SyncOutcome::NoChange => None,
        };
        // 9. 重读返回
        let fresh = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
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
    pub async fn to_inspection_core<R: PartRepoTrait>(
        repo: &mut R,
        snowflake: &SnowflakeIdGenerator,
        target_inspection_shelf_id: i64,
        batch_id: i64,
        expected_batch_version: i32,
        quantity: Option<i32>,
        note: Option<&str>,
        current: &CurrentUser,
    ) -> Result<ToXxxOut, AppError> {
        // 1. 校验品检架（target_inspection_shelf）
        let target_shelf =
            Self::_validate_inspection_shelf(repo, target_inspection_shelf_id).await?;
        // 2. 反查批次 → part_id 并读 part（2026-10-02 去 part 化，理由同 `to_ship_core`）
        let anchor = Self::_lookup_batch_by_id(repo, batch_id).await?;
        let part_id = anchor.part_id;
        let part: TPartInspected = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在"))
        })?;
        // 3. 状态机守卫：必须在 {PENDING, PROGRAMMING, IN_PROCESS}
        //
        //    2026-10-06 读 `anchor.status`（批次真源）而非 `part.status`（派生缓存列），
        //    理由见 `to_ship_core` 同处注释。本端点**当前没有**假阴性：批次源状态里
        //    IN_PROCESS 的 progress 最高，而 `t_part.status` 按 min-progress 只可能
        //    派生成更早的 PENDING / PROGRAMMING / IN_PROCESS —— 三者到 INSPECTION
        //    都有边。但判据读错列这件事本身就是隐患（下一条状态机补边时就会变成
        //    假阴性），故一并对齐，不留「恰好没事」的写法。
        let from = PartStatus::from_str(&anchor.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("batch {batch_id} 状态非法: {}", anchor.status),
            )
        })?;
        if !from.can_transition_to(PartStatus::INSPECTION) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("batch {batch_id} 当前状态 {} 不允许送检", from.as_str()),
            ));
        }
        // 5. 定位目标批次（先于 IN_PROCESS 组合校验，以便直接读 target_batch.location）
        let target = Self::_resolve_scan_target_batch(repo, part_id, batch_id).await?;
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
            Self::_split_for_partial_op(repo, snowflake, &target, quantity, current).await?;
        // 7. UPDATE t_part_batch: {PENDING, PROGRAMMING, IN_PROCESS} → INSPECTION
        //
        // 隐式多批次 rollup：前置状态守卫（step 3）已限定 from ∈ {PENDING, PROGRAMMING,
        // IN_PROCESS}，该状态下不可能存在 INSPECTION 批次，翻转 `t_part.status` 安全。
        //
        // 2026-10-01：min-progress 回填与 assembly 级联**已收进** status_gate
        // 一函数内（下方 step 8 直接取 `rollup.sync`，不再二次派生）；
        // 0 行由 gate 抛 40901 `VERSION_CONFLICT`，原 `if n == 0` 是死代码。
        let rollup = repo
            .mark_batch_inspected(
                operated_id,
                operated_version,
                target_shelf.id,
                Some(current.id),
            )
            .await?;
        // 8. batch → part → assembly rollup 结果：已在 step 7 的 status_gate 内完成
        let synced_assembly_id = match rollup.sync {
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
        repo.insert_part_event(NewPartEvent {
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
        })
        .await?;
        // 10. 重读返回
        let fresh = repo.get_part_inspected(part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} vanished"))
        })?;
        Ok(ToXxxOut {
            part: PartOut::from(fresh),
            new_batch_id: new_batch_id_out,
            synced_assembly_id,
        })
    }
}
