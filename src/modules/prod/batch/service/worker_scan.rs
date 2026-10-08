//! prod::batch 的工人扫码台主入口：`POST /api/v2/prod/batches/worker-scan`
//!
//! worker 把持有件通过扫码台 **RETURNED**（放回生产架）/ **INSPECTED**（直接送检）。
//!
//! ## 与 refill 的原子性
//! 本文件只承载 `worker_scan_event`（状态翻转 + 写事件日志）；refill 由 handler
//! 在 commit 之前同事务紧接着调 `QueueService::refill_for_worker_with_work_type`
//! —— scan 与 refill 共享一个原子事务，否则「扫描放回 → refill 抢批」中间会被
//! 并发抢走同批。
//!
//! ## 错误码契约（2026-10-10：货架由服务端自动选，`shelf_id` 入参删除）
//! - 20101 `BIZ_PART_NOT_FOUND` —— serial_no 不存在
//! - 20103 `BIZ_INVALID_TRANSITION` —— part 当前状态不允许（送检分支）
//! - 20104 `BIZ_INVALID_VALUE` —— 非法 part 状态
//! - 20114 `BIZ_PART_BATCH_NOT_HELD_BY_WORKER` —— worker 没持有 / 多批歧义
//! - 20201 `BIZ_WORKER_NOT_FOUND` —— badge_code 未注册
//! - 20202 `BIZ_WORKER_INACTIVE` —— worker 已停用
//! - 20206 `BIZ_WORKER_NO_WORK_TYPE` —— worker 未分配工种
//! - 20508 `BIZ_SHELF_PROCESS_NOT_FOUND` —— RETURNED：推导出的下一道工序**没有**
//!   可用生产货架（没配映射 / 映射的架全停用或软删 / 全非 PRODUCTION / 都不在
//!   当前账号 scope 内）
//! - 40001 `VALIDATION_ERROR` —— next_process_id 缺 / 非法（**仅非顺应工序时**
//!   才要求前端传 `next_process_id`；顺应工序时后端按链推导，链尾时直接自动送检，
//!   见 `crate::shared::batch::chain` 与 RETURNED 分支注释）
//! - 40301 `SHELF_MISMATCH` —— 送检时当前账号 scope 内**没有**任何可用的 INSPECTION
//!   货架（原先这条码守的是「前端指定的品检架越权」；选架后触发时机变成「scope 内
//!   没有品检架」，语义仍是**无权**而不是「架不存在」）
//! - 40901 `VERSION_CONFLICT` —— 乐观锁失败
//!
//! ## 2026-10-10 三处结构变化
//!
//! 1. **共享前置的 shelf 校验删除**：原来第 1 步（`event_type` 分支**之前**）无条件
//!    查 `ShelfRepo::get_by_id_zone(req.shelf_id, "PRODUCTION")`，两个分支共用一个
//!    架。现在两个分支各自选架（RETURNED 选生产架、送检选品检架），共享前置无从谈起。
//! 2. **RETURNED 与 INSPECTED 都改成「选架 + 写入」**，`ShelfProcessRepo` 与
//!    `ShelfRepo` 在本文件的引用归零（映射校验与存在/停用/zone 三谓词都已被选架覆盖）。
//! 3. **RETURNED 新增「链尾自动送检」分支**：批次在工序链上是最后一道时，放回 = 做
//!    完了，直接走送检写入路径，不落生产架。响应的 `event_type` 因此**可能与请求的
//!    不同**（发 `RETURNED` 收 `WORKER_SCAN_INSPECTED`）—— handler 按
//!    `scan_out.event_type` 广播，所以 WS 链路自动成立、无需改 handler。

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::service::SyncOutcome;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::service::PartService;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::dto::WorkerScanRequest;
use crate::modules::prod::batch::vo::WorkerScanCoreOut;
use crate::modules::prod::queue::dto::WorkerScanEvent;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::shared::error::{AppError, code};

use super::BatchService;

impl BatchService {
    /// worker-scan 共享核心（被单件端点 `POST /prod/batches/worker-scan` 调用，Task 8）。
    ///
    /// 两分支：
    /// - `RETURNED`：worker 把持有件放回生产架（同 admin_remove 语义，但走扫码台 +
    ///   worker 自查路径）；目标架由 `pick_least_loaded(PRODUCTION, Some(目标工序), scope)`
    ///   按负载选出（选不出 → 20508）；切 holder worker → shelf（OCC）+ 写
    ///   `RETURNED_TO_SHELF` 事件。**目标工序 2026-10-09 起由后端按链推导**（顺应工序
    ///   时），前端只在非顺应工序时必须显式传 `next_process_id`（详见 RETURNED 分支注释
    ///   与 `crate::shared::batch::chain`）。
    ///   **2026-10-10 新增**：批次在链尾（`chain_state == "TAIL"`）时该批已完工，
    ///   改走与 `INSPECTED` 完全相同的送检写入路径，`event_type` 响应为
    ///   `WORKER_SCAN_INSPECTED`。
    /// - `INSPECTED`：worker 把持有件直接送检（INSPECTION → INSPECTION + 状态机
    ///   IN_PROCESS → INSPECTION）；目标品检架由
    ///   `pick_least_loaded(INSPECTION, None, scope)` 按负载选出（选不出 → 40301）；
    ///   状态机校验 → mark_*_inspected（OCC）+ 写 `SENT_TO_INSPECTION` 事件。
    ///
    /// 两条路径都先定位 worker 持有的 IN_PROCESS+WORKER 批次
    /// （`find_worker_held_batch_for_part`），多批次歧义 / 没持有 → 20114
    /// BIZ_PART_BATCH_NOT_HELD_BY_WORKER。
    ///
    /// **本方法不负责 refill**：handler 在 commit 之前紧接着调
    /// `QueueService::refill_for_worker`（同事务）。这样 scan 与 refill 共享
    /// 一个原子事务（OM-6 决议），避免扫描放回 → refill 抢批中间被并发抢走
    /// 同批的竞争窗口。
    ///
    /// `WorkerScanEvent` 是 unit enum（`Copy`），所以 `req` 按值传（caller 的
    /// DTO `req.clone()` 不再需要）。
    // 2026-09-11 PR-B2 改造后保留 master 既有的 `event_type_str` 晚初始化模式（见本文件
    // `worker_scan_event` 的 RETURNED 分支
    // 是 master 既有的 pre-existing 例外），新版本 clippy (1.98) 会以
    // `clippy::needless_late_init` 报警，故显式豁免。
    #[allow(clippy::too_many_lines, clippy::needless_late_init)]
    pub async fn worker_scan_event<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: WorkerScanRequest,
        current: &CurrentUser,
    ) -> Result<WorkerScanCoreOut, AppError> {
        // 1. 2026-10-10：原来的「共享前置 shelf 校验」（无条件查 PRODUCTION 区活跃架）
        //    随 `shelf_id` 入参删除而消失 —— 两个分支现在各自选架（RETURNED 选生产架、
        //    送检选品检架），不存在「共用的那一个架」。
        // 2. 反查 worker
        let worker = WorkerRepo::get_by_badge_code(repo.conn_mut(), &req.badge_code, false)
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
        let part = repo
            .get_by_serial(&req.serial_no, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("serial_no {} 不存在", req.serial_no),
                )
            })?;
        // 4. 定位 batch（worker 持有 IN_PROCESS+WORKER）
        let bid_hint = req.batch_id.as_deref().and_then(|s| s.parse().ok());
        let batch = match repo
            .find_worker_held_batch_for_part(part.id, worker.id, bid_hint)
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
                    "multiple IN_PROCESS batches held by this worker; specify batch_id in request body"
                        .to_string(),
                ));
            }
            Err(e) => return Err(AppError::from(e)),
        };
        // 5. event_type 分支
        let event_type_str: &'static str;
        // 父装配件 id（仅当 INSPECTED 分支触发父 status 变更时 Some）；
        // RETURNED 分支 part.status 保持 IN_PROCESS，不挂 sync。
        let mut synced_assembly_id: Option<i64> = None;
        match req.event_type {
            WorkerScanEvent::RETURNED => {
                // 解析批次在链上的位置，决定「下一道工序」是自动推导还是前端显式指定。
                //
                // 2026-10-09 起这段是「一个判据、三个分支」：
                // - **链尾**（`chain_state == "TAIL"`）⇒ 这批做完了，**直接送检**，
                //   不落生产架（2026-10-10 新增）；
                // - **顺应工序**（step 指针与 `current_process_id` 一致）且链内有下一
                //   道 ⇒ 自动取链上下一 step 的工序，前端可以**不传**
                //   `next_process_id`；
                // - **非顺应**（无链 / 链已软删 / 指针漂移 / 链内工序重复）⇒
                //   必须由前端显式指定 —— 这些成因下「链上下一道」要么推不出来，要么
                //   推出来的值不可信，猜一次就是一次静默错工序。
                //
                // ⚠️ **TAIL 判定必须先于 `next_process_id` 的必填校验**：链尾是
                // 「顺应但没有下一道」，若把必填校验放在 TAIL 判定之前，工人在链尾
                // 放回时会被 40001 拦下、根本走不到送检分支（而链尾恰恰**不该**要求
                // 前端填下一道工序）。
                //
                // 判据本身（锚链两步定位、按 `current_process_id` 在链内重新定位、
                // 链内工序重复显式落 NONE）与读侧 `GET /parts/by-worker` 逐条同源：
                // 共用 `shared::batch::chain`，故「前端看到可免填」与「写侧实际推进到
                // 哪道」不会分叉（这正是 2026-09-30 起 step 指针恒 NULL 时两者会
                // 分叉的根因）。
                let position = crate::shared::batch::chain::resolve_chain_position(
                    repo.conn_mut(),
                    part.id,
                    &batch,
                )
                .await?;
                // ── 链尾自动送检（2026-10-10 新增）────────────────────────────
                //
                // 判据是 `is_pointer_consistent(&batch) && chain_state == "TAIL"` ——
                // 与紧邻其下的 NEXT 分支**同款**地要求指针一致。缺了指针一致性这一半，
                // 一个 step 指针已经漂移的批次（`current_process_step_id` 指向别的链
                // / 已软删 / NULL）只要它恰好落进一条单 step 链的锚链里，就会被判成
                // 「做完了」直接送检 —— 绕过了「非顺应 ⇒ 必须显式指定下一道工序」
                // 这道闸门。而指针漂移的成因恰恰说明链信息已经不可信，此时最不该做
                // 的是替工人断定「这批做完了」。
                if position.is_pointer_consistent(&batch)
                    && position.chain_state.as_deref() == Some("TAIL")
                {
                    // 与 `INSPECTED` 分支**逐字同款**的写入路径（同一个
                    // `sent_to_inspection` helper），故状态机守卫、OCC、
                    // part/assembly 派生、事件日志都只有一份实现。
                    synced_assembly_id =
                        sent_to_inspection(&mut repo, snowflake, &part, &batch, &worker, current)
                            .await?;
                    event_type_str = "WORKER_SCAN_INSPECTED";
                    return Ok(WorkerScanCoreOut {
                        worker_id: worker.id,
                        part_id: part.id,
                        batch_id: batch.id,
                        event_type: event_type_str.to_string(),
                        synced_assembly_id,
                        work_type_id,
                        badge_code: worker.badge_code.clone(),
                    });
                }
                let (next_pid, step_id_opt): (i64, Option<i64>) = if position
                    .is_pointer_consistent(&batch)
                    && position.chain_state.as_deref() == Some("NEXT")
                {
                    // 顺应工序：链上下一个 step 已定位到，`next_process_id` 为
                    // NULL 说明 LATERAL 的 `NEXT` 分支与派生值口径被改散了 ——
                    // 那是 SQL 内部不自洽，宁可拒收也不能放一个无工序的批次进池。
                    let pid = position.next_process_id.ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                            "链位置派生返回 NEXT 但没有下一道工序（SQL 内部不自洽）",
                        )
                    })?;
                    (pid, position.next_step_id)
                } else {
                    // 非顺应工序：必须前端显式指定。
                    let raw = req.next_process_id.as_deref().ok_or_else(|| {
                        AppError::validation(
                            "非顺应工序（无工序链 / 链已软删 / 当前工序不在链内 / 当前已是链尾），\
                                 必须在 next_process_id 里显式指定下一道工序",
                        )
                    })?;
                    let pid = raw
                        .parse()
                        .map_err(|_| AppError::validation("next_process_id 非法"))?;
                    // 链内解析该工序的 step：无链 → `None`（该列按写入不变式恒为
                    // NULL，SQL 侧 COALESCE 保留原值）；有链但链内没有这道工序 ⇒
                    // `20702`，即「链是有的，却没把正在加工的工序登记进链内」——
                    // 跟放着批次一起静默入池、之后每一步定位都漂移相比，拒收更便宜。
                    let chain_id =
                        crate::shared::batch::optional_process_chain(repo.conn_mut(), part.id)
                            .await?;
                    let step =
                        crate::shared::batch::optional_step_id(repo.conn_mut(), chain_id, pid)
                            .await?;
                    (pid, step)
                };
                // 目标生产架：2026-10-10 起由服务端按 `current_load / capacity`
                // 升序选（`shelf_id` 入参删除）。
                //
                // 「选出来的架必须映射 `next_pid`」这条 20507 守卫**已被选架本身
                // 覆盖**：选架的候选集只含 `t_shelf_process` 里映射了该工序的行
                // （带 `deleted_at IS NULL` 闸门）。所以不必再调
                // `ShelfProcessRepo::exists_for_shelf_process` 做第二次判定 ——
                // 那会变成「同一个谓词判两遍」，两遍的闸门一旦漂移就会放行已软删的映射。
                //
                // ⚠️ 选架用的是**推导出来的** `next_pid`（顺应工序时 = 链上下一道），
                // 而请求体里的 `next_process_id` 在该分支可能压根不存在。
                let shelf = crate::shared::shelf::select::pick_least_loaded(
                    repo.conn_mut(),
                    "PRODUCTION",
                    Some(next_pid),
                    crate::shared::shelf::select::shelf_scope_for(current),
                )
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_SHELF_PROCESS_NOT_FOUND,
                        format!(
                            "process {next_pid} 无可用生产货架（无 active 映射，或命中的映射其货架 \
                             均已软删 / 已停用 / 非 PRODUCTION 区 / 不在当前账号 scope 内）"
                        ),
                    )
                })?;
                // 切 holder worker → shelf（OCC）
                //
                // 2026-09-30 修复：RETURNED 是全仓唯一**推进工序**的
                // 路径 —— 工人在 P1 完工、扫 RETURNED，批次归还货架后必须落进 **P2**
                // 的候选池。此前 `mark_batch_returned` 不写
                // `current_process_id`，批次会带着 P1 落回 P1 池（migration 004 确立
                // 的「唯一权威依据」在主干流程上说谎）。
                //
                // 2026-10-09：step 指针与 `current_process_id` **成对推进** ——
                // `step_id_opt` 自此真正落库（`mark_batch_returned` 的
                // `current_process_step_id = COALESCE($5, 原值)`），批次在链内的位置
                // 指针不再停在首次定位那一步。
                //
                // ⚠️ `t_part.process_chain_id` 是**可空列**（baseline migration 001
                // 建表时 `process_chain_id bigint` 无 NOT NULL，列 COMMENT 明写
                // 「NULL = 未制定工艺链」），手工工单无链是常态。链 id 一律经
                // `shared::batch::optional_process_chain` / `optional_step_id` 取，
                // 它们按 `Option<i64>` 收（外层 `Option` 由 `fetch_optional` 表示
                // 「有没有行」，**不是**列的类型；列的可空性收在 `O` 里，末尾
                // `.flatten()` 把两层压成一层）。若改成按 `i64` 解码，
                // `t_part` 查询会在 `column 0` 上抛
                // `error occurred while decoding column 0: unexpected null; try
                // decoding as an Option` 整笔 500 —— 且因为发生在下面的 `if let`
                // **之前**，「无链时保留批次旧 step」那条分支对手写工单恒不可达。
                // 回归见
                // `tests/production/queue.rs::worker_scan_returned_without_process_chain_succeeds`。
                // 同款反模式（`Option<i64>` 包当前 `NOT NULL` 的列，列一旦变可空就
                // 同样 500）另见 `prod/shelf_process/repo.rs::find_first_shelf_for_process`。
                let n = repo
                    .mark_batch_returned(
                        batch.id,
                        batch.version,
                        // 形参语义未变：「服务端选出来的目标架」，2026-10-10 起不再
                        // 来自请求体。
                        shelf.id,
                        step_id_opt,
                        Some(next_pid),
                        Some(current.id),
                    )
                    .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                // PR-B2：part 派生列由 sync_from_batch_change 统一回填；part.status
                // 未变化（IN_PROCESS→IN_PROCESS）但 location/holder/process 物化。
                //
                // 2026-10-01 订正：RETURNED 不改 status，该批次仍
                // 非终态 → min-progress 推不出 part 终态，`event_id` 传 `None`
                // （归档事件分支不可达）。
                PartService::sync_from_batch_change(&mut repo, part.id, current, None).await?;
                repo.insert_part_event(NewPartEvent {
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
                })
                .await?;
                event_type_str = "WORKER_SCAN_RETURNED";
            }
            WorkerScanEvent::INSPECTED => {
                synced_assembly_id =
                    sent_to_inspection(&mut repo, snowflake, &part, &batch, &worker, current)
                        .await?;
                event_type_str = "WORKER_SCAN_INSPECTED";
            }
        }
        Ok(WorkerScanCoreOut {
            worker_id: worker.id,
            part_id: part.id,
            batch_id: batch.id,
            event_type: event_type_str.to_string(),
            synced_assembly_id,
            work_type_id,
            badge_code: worker.badge_code.clone(),
        })
    }
}

/// 「送检」的完整写入路径：`选品检架 → 状态机守卫 → mark_batch_inspected → 写事件`。
///
/// **两个调用点**（2026-10-10 起）：
/// 1. `WorkerScanEvent::INSPECTED` —— 工人显式送检；
/// 2. `WorkerScanEvent::RETURNED` 且 `chain_state == "TAIL"` —— 链尾自动送检。
///
/// 之所以抽成自由函数而不是让第二条路径「跳进」第一条：`match` 的两个臂在同一个
/// 作用域里共享 `synced_assembly_id` 等可变绑定，而链尾那条需要**提前 return**
/// （TAIL 分支之后不再有 RETURNED 的其余逻辑），用「设 flag + 臂内分支」会让两条
/// 路径的状态机守卫被复制一遍 —— 而守卫与事件字面量是最该只有一份的东西。
///
/// 返回 `synced_assembly_id`（父装配件真变了才有值），供 handler 决定是否广播
/// `ASSEMBLY_UPDATED`。
///
/// ## 错误码
/// - `40301 SHELF_MISMATCH`：当前账号 scope 内没有可用的 INSPECTION 货架
/// - `20103 BIZ_INVALID_TRANSITION` / `20104 BIZ_INVALID_VALUE`：批次状态不允许送检
/// - `40901 VERSION_CONFLICT`：乐观锁失败（由 `shared::batch::status` 抛）
async fn sent_to_inspection<R: PartRepoTrait>(
    repo: &mut R,
    snowflake: &SnowflakeIdGenerator,
    part: &crate::modules::part::model::TPart,
    batch: &crate::shared::batch::TPartBatch,
    worker: &crate::modules::prod::worker::model::TWorker,
    current: &CurrentUser,
) -> Result<Option<i64>, AppError> {
    // 目标品检架：按负载自动选（`target_inspection_shelf_id` 入参 2026-10-10 删除）。
    // 选不出时用 `40301` 而不是 `20501`：成因是「你的 scope 里没有任何可用的品检架」
    // （典型：一个只绑了生产架的 SHELF_ACCOUNT），语义是**无权**而不是「架不存在」。
    let target_id = crate::shared::shelf::select::pick_least_loaded(
        repo.conn_mut(),
        "INSPECTION",
        None,
        crate::shared::shelf::select::shelf_scope_for(current),
    )
    .await?
    .ok_or_else(|| crate::shared::shelf::select::no_candidate_in_scope("INSPECTION"))?
    .id;
    // 状态机：IN_PROCESS → INSPECTION
    //
    // 2026-10-06 读 `batch.status`（批次真源）而非 `part.status`（派生缓存列），
    // 理由与假阴性分析见 `transition_core.rs::to_ship_core` 同处注释。本路径
    // 当前无假阴性（batch 源状态固定 IN_PROCESS，part 派生值不会更晚），
    // 但判据读错列本身是隐患，一并对齐。
    let from = PartStatus::from_str(&batch.status).ok_or_else(|| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("batch {} 状态非法: {}", batch.id, batch.status),
        )
    })?;
    if !from.can_transition_to(PartStatus::INSPECTION) {
        return Err(AppError::biz(
            code::BIZ_INVALID_TRANSITION,
            format!("batch {} 当前状态 {} 不允许送检", batch.id, from.as_str()),
        ));
    }
    // 切 holder worker → target_shelf + 状态 IN_PROCESS → INSPECTION（OCC）
    // 2026-10-01：写 + part 派生 + assembly 级联已在 shared::batch::status 内完成，
    // 0 行由 gate 抛 40901（原 `if n == 0` 是死代码）。
    let rollup = repo
        .mark_batch_inspected(batch.id, batch.version, target_id, Some(current.id))
        .await?;
    repo.insert_part_event(NewPartEvent {
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
    })
    .await?;
    // PR-B2：直接取 gate 的派生结果（不再补调 `PartService::sync_from_batch_change`
    // —— 第二次派生必然 `NoChange`，会把 `synced_assembly_id` 恒吞成 null）。
    Ok(match rollup.sync {
        SyncOutcome::Changed(aid) => Some(aid),
        SyncOutcome::NoChange => None,
    })
}
