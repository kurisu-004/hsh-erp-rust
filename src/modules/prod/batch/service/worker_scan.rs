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
//! ## 错误码契约
//! - 20101 `BIZ_PART_NOT_FOUND` —— serial_no 不存在
//! - 20103 `BIZ_INVALID_TRANSITION` —— part 当前状态不允许（INSPECTED 分支）
//! - 20104 `BIZ_INVALID_VALUE` —— 非法 part 状态
//! - 20114 `BIZ_PART_BATCH_NOT_HELD_BY_WORKER` —— worker 没持有 / 多批歧义
//! - 20201 `BIZ_WORKER_NOT_FOUND` —— badge_code 未注册
//! - 20202 `BIZ_WORKER_INACTIVE` —— worker 已停用
//! - 20206 `BIZ_WORKER_NO_WORK_TYPE` —— worker 未分配工种
//! - 20501 `BIZ_SHELF_NOT_FOUND` —— shelf 不存在 / 非 PRODUCTION / target 非 INSPECTION
//! - 20507 `BIZ_SHELF_PROCESS_NOT_MAPPED` —— RETURNED 时 shelf ↔ process 未映射
//! - 20511 `BIZ_SHELF_NOT_INSPECTION_ZONE` —— target_inspection_shelf.zone ≠ 'INSPECTION'
//! - 40001 `VALIDATION_ERROR` —— next_process_id / target_inspection_shelf_id 缺 / 非法
//! - 40301 `SHELF_MISMATCH` —— 当前用户无权限访问 target shelf
//! - 40901 `VERSION_CONFLICT` —— 乐观锁失败

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::service::SyncOutcome;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::service::PartService;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::dto::WorkerScanRequest;
use crate::modules::prod::batch::vo::WorkerScanCoreOut;
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::modules::prod::queue::dto::WorkerScanEvent;
use crate::modules::prod::shelf_process::repo::ShelfProcessRepo;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

use super::BatchService;

impl BatchService {
    /// worker-scan 共享核心（被单件端点 `POST /prod/batches/worker-scan` 调用，Task 8）。
    ///
    /// 两分支：
    /// - `RETURNED`：worker 把持有件放回生产架（同 admin_remove 语义，但走扫码台 +
    ///   worker 自查路径）；要求 shelf ∈ PRODUCTION 区 + active；shelf ↔
    ///   next_process_id 在 `t_shelf_process` 必须有映射（20507 NOT_MAPPED）；切
    ///   holder worker → shelf（OCC）+ 写 `RETURNED_TO_SHELF` 事件。
    /// - `INSPECTED`：worker 把持有件直接送检（INSPECTION → INSPECTION + 状态机
    ///   IN_PROCESS → INSPECTION）；要求 target shelf ∈ INSPECTION 区 + active；
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
        // 1. shelf 校验（worker-scan shelf 必须 PRODUCTION 区 active；存在性 + zone 守卫）
        let _shelf = ShelfRepo::get_by_id_zone(repo.conn_mut(), req.shelf_id, "PRODUCTION")
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {} 不存在或非 PRODUCTION 区", req.shelf_id),
                )
            })?;
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
                // RETURNED 必须传 next_process_id
                let next_pid: i64 = req
                    .next_process_id
                    .as_deref()
                    .ok_or_else(|| AppError::validation("RETURNED 必须传 next_process_id"))?
                    .parse()
                    .map_err(|_| AppError::validation("next_process_id 非法"))?;
                // shelf ↔ process 映射校验。
                //
                // 必须走 `ShelfProcessRepo::exists_for_shelf_process`，**不要**在本文件
                // 内联 `SELECT EXISTS(…)`：共享方法带 `deleted_at IS NULL` 守卫，内联
                // 写法一旦漏掉，已软删的货架↔工序映射就会放行 RETURNED，使 20507
                // `BIZ_SHELF_PROCESS_NOT_MAPPED` 的触发条件与 worker-pool `move_batch`
                // 那条路径分叉。走共享方法后两条路径同源，不会再各自漂移。
                let maps = ShelfProcessRepo::exists_for_shelf_process(
                    repo.conn_mut(),
                    req.shelf_id,
                    next_pid,
                )
                .await?;
                if !maps {
                    return Err(AppError::biz(
                        code::BIZ_SHELF_PROCESS_NOT_MAPPED,
                        format!("shelf {} 不映射到工序 {}", req.shelf_id, next_pid),
                    ));
                }
                // PR-3 批次 step 化：解析 step_id（chain 内 process_id → step_id）。
                // part 的 `process_chain_id` 为 NULL（未制定工艺链）时**不能**静默抹除
                // batch.current_process_step_id（否则 part 持有件从 worker 归还到货架后
                // 丢失 step 上下文）—— 此时保留旧 step_id 值。
                //
                // ⚠️ `O` 必须是 `Option<i64>`（外层 `Option` 由 `fetch_optional` 表示
                // 「有没有行」，**不是**列的类型；列的可空性要自己收在 `O` 里，末尾
                // `.flatten()` 把两层压成一层）。
                // `t_part.process_chain_id` 是可空列：baseline migration 001 建表时
                // `process_chain_id bigint` **无 NOT NULL**，列 COMMENT 明写
                // 「NULL = 未制定工艺链」；本查询的目标列即 `column 0`，为 NULL 时按
                // `i64` 解码触发 sqlx
                // `error occurred while decoding column 0: unexpected null; try decoding as an Option`
                // 整笔 500。**真会触发**：手工工单（无工艺链）是常态，工人归还这类件
                // 必现；且 NULL 在下方 `if let` **之前**就抛，所以 `O` 若不收
                // `Option`，下面的 else 分支（保留批次旧 step）对手写工单恒不可达。
                // 回归见 `tests/production/queue.rs::worker_scan_returned_without_process_chain_succeeds`。
                // 同款反模式（`Option<i64>` 包当前 `NOT NULL` 的列，列一旦变可空就同样
                // 500）另见 `prod/shelf_process/repo.rs::find_first_shelf_for_process`。
                //
                // ⚠️ 2026-09-30 起的**已知缺口**：下面算出的 `step_id_opt`
                // 传给 `mark_batch_returned` 后**被丢弃** —— 该函数的
                // `current_process_step_id` SET 子句在 2026-09-30 prod/pool move
                // 重构中被移除（admin 主动退回不推进工序链），RETURNED 复用了同一
                // 函数，于是该显示用列在 RETURNED 时也不再更新。
                //
                // **刻意不修**：影响面仅限显示 —— 池归属已由
                // `current_process_id` 承担并正确写入；step 是「批次
                // **首次定位**在工艺链哪一步」的可选显示用信息，不是状态机依赖。
                //
                // ⚠️ 措辞订正（2026-09-30 附带发现）：step **不是**
                // 「会随流转推进的进度指针」—— 它只在首次定位工序时写、之后一律
                // 不再推进（RETURNED / INSPECTED 都不写），对多工序链工单永远停在
                // 首次定位那一步。后续单独一轮处理（届时 `mark_batch_returned` 需按
                // 调用方决定是否写 step，语义与 `advance_to_process_id` 同形）。
                let chain_id_opt: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT process_chain_id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(batch.part_id)
                .fetch_optional(repo.conn_mut())
                .await?
                .flatten();
                let step_id_opt: Option<i64> = if let Some(chain_id) = chain_id_opt {
                    ProcessChainRepo::resolve_step_id_by_process(
                        repo.conn_mut(),
                        chain_id,
                        next_pid,
                    )
                    .await?
                } else {
                    // 无工艺链（手写工单的常态）：保留 batch 旧的
                    // current_process_step_id（fallback 到入参快照）
                    batch.current_process_step_id
                };
                // 切 holder worker → shelf（OCC）
                //
                // 2026-09-30 修复：RETURNED 是全仓唯一**推进工序**的
                // 路径 —— 工人在 P1 完工、扫 RETURNED 传 next_process_id=P2，批次
                // 归还货架后必须落进 **P2** 的候选池。此前 `mark_batch_returned` 不写
                // `current_process_id`，批次会带着 P1 落回 P1 池（migration 004 确立
                // 的「唯一权威依据」在主干流程上说谎）。
                //
                // 传 `Some(next_pid)` 而非「chain 软删时 fallback 旧值」：`next_pid`
                // 已在上方通过 `t_shelf_process WHERE shelf_id=$1 AND process_id=$2`
                // 校验（真实映射到该货架的 t_process.id），且 RETURNED 的业务语义就是
                // 「这批要进 P2 池」。chain 被软删只导致解析不出 P2 对应的 step
                // （**显示用定位信息**写不了），不影响**池归属**该写成 P2 —— 若此时
                // fallback 旧值，恰好让「RETURNED 不推进工序」的缺陷换个条件复现。
                let n = repo
                    .mark_batch_returned(
                        batch.id,
                        batch.version,
                        req.shelf_id,
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
                // INSPECTED 必须传 target_inspection_shelf_id
                let target_id: i64 = req
                    .target_inspection_shelf_id
                    .as_deref()
                    .ok_or_else(|| {
                        AppError::validation("INSPECTED 必须传 target_inspection_shelf_id")
                    })?
                    .parse()
                    .map_err(|_| AppError::validation("target_inspection_shelf_id 非法"))?;
                let target = ShelfRepo::get_active_by_id(repo.conn_mut(), target_id)
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
                //
                // 2026-10-06 读 `batch.status`（批次真源）而非 `part.status`（派生缓存列），
                // 理由与假阴性分析见 `transition_core.rs::to_ship_core` 同处注释。本分支
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
                // PR-B2：直接取 gate 的派生结果（不再补调
                // `PartService::sync_from_batch_change` —— 第二次派生必然
                // `NoChange`，会把 `synced_assembly_id` 恒吞成 null）。
                synced_assembly_id = match rollup.sync {
                    SyncOutcome::Changed(aid) => Some(aid),
                    SyncOutcome::NoChange => None,
                };
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
