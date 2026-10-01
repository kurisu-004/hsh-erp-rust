//! worker_pool 域业务逻辑
//!
//! 对应 Python myERP/service/worker_pool_service.py。
//!
//! ## 阶段 worker-pool-take（Task 7）
//! - `refill_for_worker` —— admin 触发「为某 worker 从其工序池抢满 max_held_batches」循环；
//!   内部循环调 `WorkerPoolRepoTrait::take_one_from_pool`，直到池空或达到上限；
//!   每抢到一批写一条 `TAKEN_FROM_POOL` 事件日志（commit 由 handler 负责）。
//! - `compute_state` —— worker 当前持有数 + 池候选数（按工序分组）；用于 state 端点。
//!
//! ## 2026-09-30 move 重构
//! - 原 `admin_remove_held_batch`（WORKER→POOL 单边）+ `assign_batch_to_worker`（POOL→WORKER
//!   单边）合并为 `move_batch`（POOL ↔ WORKER + WORKER ↔ WORKER 三方向通用移动）；
//!   - `move_batch` 不写 `current_process_step_id`（move 不推进工序链），
//!     同样不写 `current_process_id`（2026-09-30 写入不变式：池内移动工序不变）；
//!   - `from` / `to` 必须与 batch 当前 `(location, current_holder_id)` 状态一致
//!     → 不一致抛 `20122 BIZ_BATCH_LOCATION_MISMATCH`；
//!   - 同 kind 移动（POOL→POOL / WORKER→WORKER 仅源 ≠ 目标）抛 `40001 VALIDATION_ERROR`。
//!
//! ## 阶段 worker-pool-by-process（Task 3）
//! - `pool_by_process` —— admin 按工序查看候选池：process 元数据 + 映射工种 + 可执行工人 +
//!   所有货架候选批次（4 子查询合一，纯读，4 路 `&mut *conn` 复用同一事务）。
//!
//! ## 事务边界（2026-09-22 D-2 重构对齐 iam / shelf / worker 范本）
//! 事务移交 handler：handler 显式 `pool.begin()` / `commit()`，service 仅业务逻辑。
//! 所有跨 repo 操作经 `WorkerPoolRepoTrait`（胖 trait = 本域 4 + 跨域 helper 14），
//! service 公共方法签名收 `conn: &mut PgConnection`，内部 reborrow `&mut *conn` 喂 trait。
//!
//! ## Service 形态（2026-09-22 D-2 决策）
//! `WorkerPoolService` 保持 unit struct（**不**持字段依赖）。snowflake 由每个写方法
//! 形参显式收（与原 `pub struct WorkerPoolService;` + 旧方法签名兼容）——
//! 既有跨模块调用点（`part/handler/inspection.rs:298` 的 worker-scan 路径）以
//! `WorkerPoolService::refill_for_worker_with_work_type(&mut tx, &state.snowflake, ...)`
//! 形式直调 service，本任务**不修改 part 域代码**，故保留 ZST 静态 + 显式 snowflake
//! 形参的旧形态。后续 D-6 part 重构时再统一改 trait 注入式 + `Arc<WorkerPoolService>`
//! 持 snowflake 字段。
//!
//! ## PartService::sync_from_batch_change 兼容性（2026-09-22 D-2 决策）
//! PartService 仍是旧 `&mut PgConnection` 形参（part 域 D-6 未做），是关联函数
//! `PartService::sync_from_batch_change(&mut PgConnection, ...)`，service 公共
//! 方法签名收 `&mut PgConnection`，与 PartService 形参直接匹配。
//!
//! ## 2026-10-02 `t_shelf_process` SQL 收口
//! `move_batch` WORKER→POOL 分支的货架映射存在性检查由内联 SQL 改调
//! `prod::shelf_process::repo::ShelfProcessRepo::exists_for_shelf_process`
//! （`SELECT EXISTS(…)`，与原恒真式写法语义等价；20507 `BIZ_SHELF_PROCESS_NOT_MAPPED`
//! 的触发条件与文案不变）。下方 `#[cfg(test)]` 集成测试 fixture helper 里的
//! `INSERT INTO t_shelf_process` 保持原样（测试数据落库，不属 SQL 真源收口范围）。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::service::PartService;
use crate::modules::prod::shelf_process::repo::ShelfProcessRepo;
use crate::modules::prod::worker_pool::repo::WorkerPoolRepoTrait;
use crate::shared::error::{AppError, code};

use super::dto::{
    AutoAllocateMode, AutoAllocateRequest, MoveLocation, MoveRequest, ProcessBatchCount,
    WorkerPoolCountsOut,
};
use super::model::{ProcessPoolCount, RefillResult, TakenItem, WorkerPoolState};
use super::vo::{
    AutoAllocateResult, MoveResult, PoolBatchItem, ProcessPoolDetail, WorkTypeMaxHeld, WorkerBrief,
    WorkerFillItem,
};

/// worker_pool 域 service（2026-09-22 D-2 重构后）
///
/// 2026-09-22 D-2 决策：本 service 保持 unit struct（**不**持字段依赖），与
/// 原 `pub struct WorkerPoolService;` 一致。snowflake 由每个写方法形参显式收——
/// handler 端调用时传 `&state.snowflake`，跨域调用点（如 `part/handler/inspection.rs`
/// 的 worker-scan 路径）也按相同形参顺序传，不破坏既有调用点。
///
/// 这种"无字段 service + 显式 snowflake 形参"的形态与 iam / shelf / customer /
/// worker 等其它域不同——本域**唯一**需要 snowflake 的点是 part_event.id 生成
/// （其它域的事件 id 都用 caller 提供的 id）；为了保留跨模块 ZST 静态调用点
/// 兼容（`part/handler/inspection.rs:298`），暂保持显式 snowflake 形参。
///
/// 后续 D-6 part 重构时一并改用 `Arc<WorkerPoolService>` 持 snowflake 字段。
pub struct WorkerPoolService;

impl WorkerPoolService {
    /// 构造（空 struct，无字段；保留供未来切到 `Arc<WorkerPoolService>` 时使用）。
    pub fn new() -> Self {
        Self
    }

    /// 为 worker 从其货架候选池抢满 `max_held_batches`。
    ///
    /// 流程：
    /// 1. 取 worker（带 work_type_id），校验 `is_active`；
    /// 2. 取 work_type（必须设置 `max_held_batches`）；
    /// 3. 取工种可加工工序 id 列表（process_ids），空 → 业务错
    ///    `BIZ_WORK_TYPE_NO_PROCESS_MAPPING`；
    /// 4. 循环调 `WorkerPoolRepoTrait::take_one_from_pool`，每抢到一批写
    ///    `TAKEN_FROM_POOL` 事件日志 + `PartService::sync_from_batch_change` 同步
    ///    part 派生列（事务内由 handler commit）；
    /// 5. 返回 `RefillResult { worker_id, shelf_id, taken, pool_empty }`。
    #[allow(clippy::too_many_arguments)]
    pub async fn refill_for_worker(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        worker_id: i64,
        shelf_id: i64,
        operator_user_id: i64,
        current: &CurrentUser,
    ) -> Result<RefillResult, AppError> {
        let worker = (&mut *conn)
            .worker_get_by_id(worker_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORKER_NOT_FOUND,
                    format!("worker {worker_id} 不存在"),
                )
            })?;
        if !worker.is_active {
            return Err(AppError::biz(
                code::BIZ_WORKER_INACTIVE,
                format!("worker {worker_id} 已停用"),
            ));
        }
        let work_type_id = worker.work_type_id.ok_or_else(|| {
            AppError::biz(
                code::BIZ_WORKER_NO_WORK_TYPE,
                format!("worker {worker_id} 未分配工种"),
            )
        })?;

        Self::refill_for_worker_with_work_type(
            conn,
            snowflake,
            worker_id,
            work_type_id,
            shelf_id,
            &worker.badge_code,
            operator_user_id,
            current,
        )
        .await
    }

    /// 内部 helper：caller 已 fetch worker（并已校验 is_active / work_type_id），
    /// 直接接受 `work_type_id` + `badge_code`，跳过 `worker_get_by_id` 重复查询。
    ///
    /// 由 [`refill_for_worker`]（admin 路径：自己 fetch）与
    /// `part/handler/inspection.rs:298`（worker-scan 路径：service 已在 scan 步骤
    /// fetch 过 worker）共用。
    #[allow(clippy::too_many_arguments)]
    pub async fn refill_for_worker_with_work_type(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        worker_id: i64,
        work_type_id: i64,
        shelf_id: i64,
        badge_code: &str,
        operator_user_id: i64,
        current: &CurrentUser,
    ) -> Result<RefillResult, AppError> {
        let work_type = (&mut *conn)
            .work_type_get_by_id(work_type_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORK_TYPE_NOT_FOUND,
                    format!("work_type {work_type_id} 不存在"),
                )
            })?;
        // 存在性校验：NULL → 业务错（cap 由 SQL CTE 强制，不在 Rust 端使用）
        let _ = work_type.max_held_batches.ok_or_else(|| {
            AppError::biz(
                code::BIZ_WORK_TYPE_MAX_HELD_NOT_SET,
                format!("work_type {work_type_id} max_held_batches 未设置"),
            )
        })?;

        let process_ids = (&mut *conn)
            .work_type_list_process_ids(work_type_id)
            .await?;
        if process_ids.is_empty() {
            return Err(AppError::biz(
                code::BIZ_WORK_TYPE_NO_PROCESS_MAPPING,
                format!("work_type {work_type_id} 未映射工序"),
            ));
        }

        let mut taken = Vec::new();
        while let Some(t) = (&mut *conn)
            .take_one_from_pool(worker_id, shelf_id, &process_ids, operator_user_id)
            .await?
        {
            // part_event.id 用 snowflake 生成（与既有 `PartRepo::insert_part_event` 调用约定一致）
            let event_id = snowflake.next_id();
            (&mut *conn)
                .part_insert_part_event(
                    event_id,
                    t.part_id,
                    "TAKEN_FROM_POOL",
                    Some("IN_PROCESS"),
                    Some("IN_PROCESS"),
                    Some(t.batch_id),
                    Some(t.quantity),
                    Some(&t.drawing_no),
                    Some(badge_code),
                    None,
                    Some(operator_user_id),
                )
                .await?;
            // PR-B2：part 派生列（location/holder）由 sync_from_batch_change 统一
            // 回填（worker_id → worker holder，location → 'WORKER'）。
            PartService::sync_from_batch_change_with_conn(&mut *conn, t.part_id, current).await?;
            taken.push(t);
        }

        let pool_empty = taken.is_empty();
        Ok(RefillResult {
            worker_id,
            shelf_id,
            taken,
            pool_empty,
        })
    }

    /// 算 worker 当前 state：worker 元数据 + 持有数 + 池候选数（按工序）。
    ///
    /// 池候选数 = `t_part_batch` 中 `status='IN_PROCESS' AND location='PRODUCTION_SHELF'
    /// AND current_holder_id = shelf_id AND next_process_id = pid` 的批次数。
    /// 该 SQL 通过 trait helper `count_pool_by_shelf_and_process` 下沉。
    ///
    /// worker 无 work_type 时 `max_held = 0`、`process_ids = []`（state 端点不会拒绝，
    /// 仅展示空池 + 0 上限）。
    pub async fn compute_state(
        conn: &mut PgConnection,
        worker_id: i64,
        shelf_id: i64,
    ) -> Result<WorkerPoolState, AppError> {
        let worker = (&mut *conn)
            .worker_get_by_id(worker_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_WORKER_NOT_FOUND, "worker 不存在"))?;
        let work_type = if let Some(wt_id) = worker.work_type_id {
            (&mut *conn).work_type_get_by_id(wt_id).await?
        } else {
            None
        };
        let max_held = work_type
            .as_ref()
            .and_then(|w| w.max_held_batches)
            .unwrap_or(0);
        let current_held = (&mut *conn)
            .part_batch_count_held_by_worker(worker_id)
            .await?;
        let capacity_remaining = (max_held as i64 - current_held).max(0) as i32;

        let process_ids = if let Some(wt_id) = worker.work_type_id {
            (&mut *conn).work_type_list_process_ids(wt_id).await?
        } else {
            vec![]
        };

        let mut pool_count_by_process = Vec::with_capacity(process_ids.len());
        for pid in &process_ids {
            let n = (&mut *conn)
                .count_pool_by_shelf_and_process(shelf_id, *pid)
                .await?;
            pool_count_by_process.push(ProcessPoolCount {
                process_id: *pid,
                pool_count: n,
            });
        }

        // 2026-09-14 follow-up-ux 新增 → follow-up-round2 升级为 17 字段 HeldBatchItem：
        // worker 当前持有的完整 batch 列表（JOIN 6 表：t_part_batch + t_part +
        // t_customer L1+L2 + t_applicant + t_shelf）。命中 ix_t_part_batch_holder_location。
        // 2026-09-14 review 第 1 轮下沉：本查询已从 part_batch/repo.rs 迁到
        // worker_pool/repo.rs（owner 同域 + part_batch 行数 ≤1000）。
        let held_batches = (&mut *conn)
            .list_held_by_worker_with_part(worker_id)
            .await?;

        Ok(WorkerPoolState {
            worker_id,
            worker_name: worker.name,
            work_type_code: work_type.map(|w| w.code).unwrap_or_default(),
            max_held,
            current_held,
            capacity_remaining,
            pool_count_by_process,
            held_batches,
        })
    }

    /// `POST /api/v2/prod/pool/move` 业务逻辑（2026-09-30 新增）。
    ///
    /// 通用移动端点：覆盖 POOL ↔ WORKER + WORKER ↔ WORKER 三方向；取代原
    /// `admin_remove_held_batch`（WORKER→POOL）+ `assign_batch_to_worker`（POOL→WORKER）
    /// 两个单边端点。同 kind 移动（POOL→POOL）视为非法。
    ///
    /// 流程：
    /// 1. 角色守卫：Manager（service 内）
    /// 2. 同 kind 移动校验（POOL→POOL / WORKER→WORKER 同位置）→ `40001`
    /// 3. 取 batch（`status='IN_PROCESS'` + 未软删）→ 不存在 → `20121 BIZ_BATCH_NOT_FOUND`
    /// 4. 校验 `from` 与 batch 当前 `(location, current_holder_id)` 一致：
    ///    - POOL  → `(location='PRODUCTION_SHELF', current_holder_id=shelf_id)`
    ///    - WORKER → `(location='WORKER', current_holder_id=worker_id)`
    ///    - 不一致 → `20122 BIZ_BATCH_LOCATION_MISMATCH`
    /// 5. 校验 `to`：
    ///    - POOL → shelf 必须映射 `batch.current_process_id`（2026-09-30：直读
    ///      工序归属新列，不再经 `current_process_step_id` → step JOIN 中转）
    ///    - WORKER → worker is_active 且工序资格 + 容量（`held < max_held`）
    /// 6. 按 (from, to) 选 SQL：
    ///    - POOL → WORKER：复用 `take_specific_from_pool`（service 入口已 fetch batch，
    ///      走 OCC `WHERE version = $exp` 单 SQL 原子切换）
    ///    - WORKER → POOL：复用 `part_mark_batch_returned`（**去掉 step 写入**，
    ///      2026-09-30 重构）
    ///    - WORKER → WORKER：新加 `move_worker_to_worker`（同样不写 step）
    /// 7. 写 part_event（`MOVED` 类型，note 含 from→to 描述）
    /// 8. `PartService::sync_from_batch_change` 同步 part 派生列
    /// 9. 返回 `MoveResult { batch_id, from_kind, to_kind, new_holder_id, new_location,
    ///    version, current_held?, max_held?, shelf_id?, taken? }`
    ///
    /// 关键不变量（plan §2.3）：
    /// - 所有 move SQL **不写** `current_process_step_id`（工序链不被破坏）
    /// - 所有 move SQL **不写** `current_process_id`（2026-09-30 写入不变式：
    ///   池内移动工序不变，批次归还货架后仍属原工序候选池）
    /// - OCC：`UPDATE ... WHERE version = $exp`，0 行 → `40901 VERSION_CONFLICT`
    /// - `from` 必与 batch 当前状态匹配（→ 40904）
    #[allow(clippy::too_many_arguments)]
    pub async fn move_batch(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: MoveRequest,
        current: &CurrentUser,
    ) -> Result<MoveResult, AppError> {
        current.require_role(Role::Manager)?;

        let from_kind = match &req.from {
            MoveLocation::Pool { .. } => "POOL",
            MoveLocation::Worker { .. } => "WORKER",
        };
        let to_kind = match &req.to {
            MoveLocation::Pool { .. } => "POOL",
            MoveLocation::Worker { .. } => "WORKER",
        };

        // 2. POOL→POOL 同 kind 移动 → 非法（WORKER→WORKER 是合法方向，需走 §5 三方向分支）
        if from_kind == "POOL" && to_kind == "POOL" {
            return Err(AppError::validation(format!(
                "move 同 kind 移动非法（from={from_kind} to={to_kind}）；应跨 kind 移动"
            )));
        }

        // 3. 取 batch（include_deleted=false，已软删视为不存在）
        let batch = (&mut *conn)
            .part_batch_get_by_id(req.batch_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_BATCH_NOT_FOUND,
                    format!("batch {} 不存在或已软删", req.batch_id),
                )
            })?;
        if batch.status != "IN_PROCESS" {
            return Err(AppError::biz(
                code::BIZ_BATCH_INVALID_STATUS,
                format!(
                    "batch {} 当前 status='{}'，不允许 move（要求 'IN_PROCESS'）",
                    batch.id, batch.status
                ),
            ));
        }

        // 4. from 与 batch 当前 (location, holder) 匹配校验
        match &req.from {
            MoveLocation::Pool { shelf_id } => {
                let loc = batch.location.as_deref().unwrap_or("");
                if loc != "PRODUCTION_SHELF" {
                    return Err(AppError::biz(
                        code::BIZ_BATCH_LOCATION_MISMATCH,
                        format!(
                            "batch {} 当前 location='{}'，from.kind=POOL 期望 'PRODUCTION_SHELF'",
                            batch.id, loc
                        ),
                    ));
                }
                if batch.current_holder_id != Some(*shelf_id) {
                    return Err(AppError::biz(
                        code::BIZ_BATCH_LOCATION_MISMATCH,
                        format!(
                            "batch {} current_holder_id={:?}，from.shelf_id={} 不匹配",
                            batch.id, batch.current_holder_id, shelf_id
                        ),
                    ));
                }
            }
            MoveLocation::Worker { worker_id } => {
                let loc = batch.location.as_deref().unwrap_or("");
                if loc != "WORKER" {
                    return Err(AppError::biz(
                        code::BIZ_BATCH_LOCATION_MISMATCH,
                        format!(
                            "batch {} 当前 location='{}'，from.kind=WORKER 期望 'WORKER'",
                            batch.id, loc
                        ),
                    ));
                }
                if batch.current_holder_id != Some(*worker_id) {
                    return Err(AppError::biz(
                        code::BIZ_BATCH_LOCATION_MISMATCH,
                        format!(
                            "batch {} current_holder_id={:?}，from.worker_id={} 不匹配",
                            batch.id, batch.current_holder_id, worker_id
                        ),
                    ));
                }
            }
        }

        // 5. to 校验 + 6. SQL 分支
        // 分支决策矩阵：
        //   POOL→WORKER: take_specific_from_pool（不写 step）+ worker 资格/容量校验
        //   WORKER→POOL: part_mark_batch_returned + shelf 映射校验
        //   WORKER→WORKER: move_worker_to_worker + 目标 worker 资格/容量校验
        let part = (&mut *conn)
            .part_get_by_id(batch.part_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("batch {} 关联 part {} 不存在", req.batch_id, batch.part_id),
                )
            })?;

        // 取 batch 当前所属工序（POOL→WORKER 与 WORKER→POOL 的 target 校验都需要）
        //
        // 2026-09-30 修复（migration 004）：原先是
        // `batch.current_process_step_id → process_chain_step_get_process_id(step_id)`，
        // 即**一次额外的 DB 往返**只为把 step_id 翻成 process_id。改直读
        // `batch.current_process_id`（工序池归属的权威列），查询整段删掉。
        let step_process_id: Option<i64> = batch.current_process_id;

        // 取 worker 元数据（事件日志 badge_code；POOL→WORKER / WORKER→WORKER 需要源 worker）
        let src_worker_badge: Option<String> = if let MoveLocation::Worker { worker_id } = &req.from
        {
            let w = (&mut *conn)
                .worker_get_by_id(*worker_id, false)
                .await?
                .ok_or_else(|| AppError::biz(code::BIZ_WORKER_NOT_FOUND, "源 worker 不存在"))?;
            Some(w.badge_code)
        } else {
            None
        };

        // 5/6 主体分支
        // 三个分支必填 new_holder_id / new_location；其余为可选填充（按 to_kind 决定）
        let new_holder_id: i64;
        let new_location: &str;
        let mut current_held: Option<i32> = None;
        let mut max_held: Option<i32> = None;
        let mut shelf_id_opt: Option<i64> = None;
        let mut taken_opt: Option<TakenItem> = None;

        match (&req.from, &req.to) {
            (MoveLocation::Pool { shelf_id }, MoveLocation::Worker { worker_id }) => {
                // POOL → WORKER：复用 take_specific_from_pool（不写 step）
                // worker 资格 + 容量校验
                let worker = (&mut *conn)
                    .worker_get_by_id(*worker_id, false)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(code::BIZ_WORKER_NOT_FOUND, "目标 worker 不存在")
                    })?;
                if !worker.is_active {
                    return Err(AppError::biz(
                        code::BIZ_WORKER_INACTIVE,
                        format!("目标 worker {worker_id} 已停用"),
                    ));
                }
                let work_type_id = worker.work_type_id.ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_WORKER_NO_WORK_TYPE,
                        format!("目标 worker {worker_id} 未分配工种"),
                    )
                })?;
                let work_type = (&mut *conn)
                    .work_type_get_by_id(work_type_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_WORK_TYPE_NOT_FOUND,
                            format!("work_type {work_type_id} 不存在"),
                        )
                    })?;
                let max_held_val = work_type.max_held_batches.ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_WORK_TYPE_MAX_HELD_NOT_SET,
                        format!("work_type {work_type_id} max_held_batches 未设置"),
                    )
                })?;

                // 工序资格校验：worker 的 work_type 必须包含 batch 当前所属工序
                if let Some(spid) = step_process_id {
                    let process_ids = (&mut *conn)
                        .work_type_list_process_ids(work_type_id)
                        .await?;
                    if !process_ids.contains(&spid) {
                        return Err(AppError::biz(
                            code::BIZ_INVALID_VALUE,
                            format!("worker {worker_id} 工种不含工序 {spid}（batch 当前工序）"),
                        ));
                    }
                }

                let current_held_val = (&mut *conn)
                    .part_batch_count_held_by_worker(*worker_id)
                    .await?;
                if current_held_val >= max_held_val as i64 {
                    return Err(AppError::biz(
                        code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED,
                        format!(
                            "worker {worker_id} 已持有 {current_held_val} 批次，工种上限 {max_held_val} 触顶"
                        ),
                    ));
                }

                // take_specific_from_pool（OCC + WHERE version = candidate.version）
                // 0 行 → 与 WORKER→POOL / WORKER→WORKER 分支统一返 40901 VERSION_CONFLICT
                // （plan §2.3「OCC 0 行 → 40901 VERSION_CONFLICT」）；
                // from 已在 §4 校验过 holder 匹配，故此处 0 行只能是被并发改版本。
                // 2026-09-30 review 第 1 轮：原返 BIZ_BATCH_LOCATION_MISMATCH (20122)，
                // 与同函数其它 0 行分支语义不一致，统一回滚为 VERSION_CONFLICT。
                let taken = (&mut *conn)
                    .take_specific_from_pool(*worker_id, *shelf_id, batch.id, current.id)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(
                            code::VERSION_CONFLICT,
                            format!(
                                "take_specific_from_pool 0 行：batch {} 已被并发改动",
                                batch.id
                            ),
                        )
                    })?;

                PartService::sync_from_batch_change_with_conn(&mut *conn, taken.part_id, current)
                    .await?;

                // 写 part_event
                let event_id = snowflake.next_id();
                (&mut *conn)
                    .part_insert_part_event(
                        event_id,
                        taken.part_id,
                        "MOVED",
                        Some("IN_PROCESS"),
                        Some("IN_PROCESS"),
                        Some(taken.batch_id),
                        Some(taken.quantity),
                        Some(&taken.drawing_no),
                        Some(&worker.badge_code),
                        Some(req.note.as_deref().unwrap_or("move POOL→WORKER")),
                        Some(current.id),
                    )
                    .await?;

                new_holder_id = *worker_id;
                new_location = "WORKER";
                current_held = Some((current_held_val + 1) as i32);
                max_held = Some(max_held_val);
                shelf_id_opt = Some(*shelf_id);
                taken_opt = Some(taken);
            }

            (
                MoveLocation::Worker {
                    worker_id: src_worker_id,
                },
                MoveLocation::Pool { shelf_id },
            ) => {
                // WORKER → POOL：复用 part_mark_batch_returned（不写 step）
                // shelf 映射校验：shelf 必须映射 batch 当前所属工序
                if let Some(spid) = step_process_id {
                    // 2026-10-02 SQL 收口：原内联
                    // `SELECT shelf_id FROM t_shelf_process WHERE shelf_id=$1 AND
                    //  process_id=$2 AND deleted_at IS NULL ORDER BY … LIMIT 1` +
                    // `is_none()` 判定（恒真式写法），改调 SQL 真源
                    // `prod::shelf_process::repo::ShelfProcessRepo::exists_for_shelf_process`
                    // 的 `SELECT EXISTS(…)` 显式存在性检查（语义等价，错误码 20507
                    // 与文案不变）。
                    let mapped =
                        ShelfProcessRepo::exists_for_shelf_process(&mut *conn, *shelf_id, spid)
                            .await?;
                    if !mapped {
                        return Err(AppError::biz(
                            code::BIZ_SHELF_PROCESS_NOT_MAPPED,
                            format!("shelf {shelf_id} 未映射工序 {spid}（batch 当前工序）"),
                        ));
                    }
                }

                let rows = (&mut *conn)
                    .part_mark_batch_returned(
                        batch.id,
                        batch.version,
                        *shelf_id,
                        None, // 2026-09-30 重构：move 不写 step
                        Some(current.id),
                    )
                    .await?;
                if rows == 0 {
                    return Err(AppError::biz(
                        code::VERSION_CONFLICT,
                        format!(
                            "batch {} 版本冲突或状态非 IN_PROCESS+WORKER（move WORKER→POOL）",
                            batch.id
                        ),
                    ));
                }

                PartService::sync_from_batch_change_with_conn(&mut *conn, part.id, current).await?;

                let event_id = snowflake.next_id();
                let badge = src_worker_badge.as_deref().unwrap_or("");
                (&mut *conn)
                    .part_insert_part_event(
                        event_id,
                        part.id,
                        "MOVED",
                        Some("IN_PROCESS"),
                        Some("IN_PROCESS"),
                        Some(batch.id),
                        Some(batch.quantity),
                        Some(&part.drawing_no),
                        Some(badge),
                        Some(req.note.as_deref().unwrap_or("move WORKER→POOL")),
                        Some(current.id),
                    )
                    .await?;

                new_holder_id = *shelf_id;
                new_location = "PRODUCTION_SHELF";
                shelf_id_opt = Some(*shelf_id);
                // 释放源 worker 当前持有一条（current_held 语义仅在 to=WORKER 时填）
                let _ = src_worker_id; // 显式标注使用
            }

            (
                MoveLocation::Worker {
                    worker_id: src_worker_id,
                },
                MoveLocation::Worker {
                    worker_id: dst_worker_id,
                },
            ) => {
                // WORKER → WORKER：新加 move_worker_to_worker（不写 step）
                if src_worker_id == dst_worker_id {
                    return Err(AppError::validation(
                        "move WORKER→WORKER 同 worker 移动非法（src == dst）",
                    ));
                }
                // 目标 worker 资格 + 容量校验（与 POOL→WORKER 同形）
                let dst_worker = (&mut *conn)
                    .worker_get_by_id(*dst_worker_id, false)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(code::BIZ_WORKER_NOT_FOUND, "目标 worker 不存在")
                    })?;
                if !dst_worker.is_active {
                    return Err(AppError::biz(
                        code::BIZ_WORKER_INACTIVE,
                        format!("目标 worker {dst_worker_id} 已停用"),
                    ));
                }
                let work_type_id = dst_worker.work_type_id.ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_WORKER_NO_WORK_TYPE,
                        format!("目标 worker {dst_worker_id} 未分配工种"),
                    )
                })?;
                let work_type = (&mut *conn)
                    .work_type_get_by_id(work_type_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_WORK_TYPE_NOT_FOUND,
                            format!("work_type {work_type_id} 不存在"),
                        )
                    })?;
                let max_held_val = work_type.max_held_batches.ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_WORK_TYPE_MAX_HELD_NOT_SET,
                        format!("work_type {work_type_id} max_held_batches 未设置"),
                    )
                })?;

                if let Some(spid) = step_process_id {
                    let process_ids = (&mut *conn)
                        .work_type_list_process_ids(work_type_id)
                        .await?;
                    if !process_ids.contains(&spid) {
                        return Err(AppError::biz(
                            code::BIZ_INVALID_VALUE,
                            format!("worker {dst_worker_id} 工种不含工序 {spid}（batch 当前工序）"),
                        ));
                    }
                }

                let current_held_val = (&mut *conn)
                    .part_batch_count_held_by_worker(*dst_worker_id)
                    .await?;
                if current_held_val >= max_held_val as i64 {
                    return Err(AppError::biz(
                        code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED,
                        format!(
                            "目标 worker {dst_worker_id} 已持有 {current_held_val} 批次，工种上限 {max_held_val} 触顶"
                        ),
                    ));
                }

                let rows = (&mut *conn)
                    .move_worker_to_worker(
                        batch.id,
                        *src_worker_id,
                        *dst_worker_id,
                        batch.version,
                        Some(current.id),
                    )
                    .await?;
                if rows == 0 {
                    return Err(AppError::biz(
                        code::VERSION_CONFLICT,
                        format!(
                            "batch {} 版本冲突或状态非 IN_PROCESS+WORKER（move WORKER→WORKER）",
                            batch.id
                        ),
                    ));
                }

                PartService::sync_from_batch_change_with_conn(&mut *conn, part.id, current).await?;

                let event_id = snowflake.next_id();
                let badge = src_worker_badge.as_deref().unwrap_or("");
                (&mut *conn)
                    .part_insert_part_event(
                        event_id,
                        part.id,
                        "MOVED",
                        Some("IN_PROCESS"),
                        Some("IN_PROCESS"),
                        Some(batch.id),
                        Some(batch.quantity),
                        Some(&part.drawing_no),
                        Some(badge),
                        Some(req.note.as_deref().unwrap_or("move WORKER→WORKER")),
                        Some(current.id),
                    )
                    .await?;

                new_holder_id = *dst_worker_id;
                new_location = "WORKER";
                current_held = Some((current_held_val + 1) as i32);
                max_held = Some(max_held_val);
            }
            // 同 kind 已在前面 ② 拦截；显式 unreachable 分支让编译器穷尽性检查通过
            #[allow(unreachable_patterns)]
            (MoveLocation::Pool { .. }, MoveLocation::Pool { .. })
            | (MoveLocation::Worker { .. }, MoveLocation::Worker { .. }) => {
                unreachable!("同 kind 移动已在 ② 拦截")
            }
        }

        Ok(MoveResult {
            batch_id: batch.id,
            from_kind: from_kind.to_string(),
            to_kind: to_kind.to_string(),
            new_holder_id,
            new_location: new_location.to_string(),
            version: batch.version + 1,
            current_held,
            max_held,
            shelf_id: shelf_id_opt,
            taken: taken_opt,
        })
    }

    /// `GET /api/v2/worker-pool/{process_id}` 业务逻辑。
    ///
    /// 流程：
    /// 1. 角色守卫：`Manager + Clerk + Inspector`（admin 视角但不止 Manager）；
    /// 2. 取 process 元数据（code + name），不存在 → `20801 BIZ_PROCESS_NOT_FOUND`；
    /// 3. 取该 process 映射的 work_type 列表（含 max_held_batches）；
    /// 4. 取可执行该 process 的 active worker 列表（含 work_type_code）；
    /// 5. 取该 process 在所有生产货架上的候选批次（JOIN 5 表）；
    /// 6. 装 `ProcessPoolDetail` 返回。
    ///
    /// 全部读操作，单事务只读，无 WS 广播，无 commit 副作用。
    pub async fn pool_by_process(
        conn: &mut PgConnection,
        current: &CurrentUser,
        process_id: i64,
    ) -> Result<ProcessPoolDetail, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        // 1. process 元数据
        let process = (&mut *conn)
            .process_get_by_id(process_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process {process_id} 不存在"),
                )
            })?;

        // 2. work_types
        let work_types = (&mut *conn)
            .work_type_list_work_types_by_process_id(process_id)
            .await?;
        let work_types = work_types
            .into_iter()
            .map(|(id, code, name, max_held_batches)| WorkTypeMaxHeld {
                work_type_id: id,
                work_type_code: code,
                work_type_name: name,
                max_held_batches,
            })
            .collect();

        // 3. workers
        let worker_rows = (&mut *conn)
            .worker_list_active_by_process_id(process_id)
            .await?;
        let workers = worker_rows
            .into_iter()
            .map(
                |(worker_id, name, work_type_id, work_type_code)| WorkerBrief {
                    worker_id,
                    name,
                    work_type_id,
                    work_type_code,
                },
            )
            .collect();

        // 4. candidates
        let items: Vec<PoolBatchItem> = (&mut *conn)
            .list_candidates_by_process_all_shelves(process_id)
            .await?;
        let total = items.len() as i64;

        Ok(ProcessPoolDetail {
            process_id,
            process_code: process.code,
            process_name: process.name,
            workers,
            work_types,
            total,
            items,
        })
    }

    /// `GET /api/v2/prod/worker-pool/counts` 业务逻辑。
    ///
    /// 2026-09-30 新增：admin 视角的全工序候选批次聚合（dashboard 快照型查询）。
    /// 流程：
    /// 1. 角色守卫：`Manager + Clerk + Inspector`（与 `pool_by_process` 同集
    ///    —— admin 视角但不止 Manager；service 内守卫）；
    /// 2. 调 `group_count_by_process_all_shelves` 单 SQL GROUP BY 取
    ///    `(process_id, count)`；
    /// 3. 二次调 `process_list_by_ids` 取 process_code / process_name 元数据；
    /// 4. 装 `WorkerPoolCountsOut` 返回（total = `counts.iter().map(|c| c.count).sum()`）。
    ///
    /// 全部读操作，单事务只读，无 WS 广播（counts 是 dashboard 快照型查询，
    /// 无业务流转，2026-09-30 spec 明确不发 WS）。process_id 顺序沿用 repo
    /// GROUP BY `ORDER BY s.process_id ASC`（稳定排序，前端按 id 稳定展示）。
    pub async fn pool_counts_all_shelves(
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<WorkerPoolCountsOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        // 1. 单 SQL GROUP BY 取 (process_id, count)
        let counts_raw: Vec<(i64, i64)> = (&mut *conn).group_count_by_process_all_shelves().await?;

        if counts_raw.is_empty() {
            return Ok(WorkerPoolCountsOut {
                counts: vec![],
                total: 0,
            });
        }

        // 2. 二次取 process 元数据
        let process_ids: Vec<i64> = counts_raw.iter().map(|(pid, _)| *pid).collect();
        let processes = (&mut *conn).process_list_by_ids(&process_ids).await?;
        // process_id → (code, name) 索引（list_by_ids 已 ORDER BY id ASC）
        let mut meta: std::collections::HashMap<i64, (String, String)> = processes
            .into_iter()
            .map(|p| (p.id, (p.code, p.name)))
            .collect();

        // 3. 装 ProcessBatchCount（保留 repo GROUP BY 的 process_id ASC 顺序）
        let mut total = 0i64;
        let counts: Vec<ProcessBatchCount> = counts_raw
            .into_iter()
            .map(|(pid, count)| {
                total += count;
                let (code, name) = meta.remove(&pid).unwrap_or_else(|| {
                    // 防御：repo GROUP BY 返回的 process_id 在 t_process 已软删
                    // → 退化为空串 + 显式 id（前端「process 已删除」占位）。
                    // 业务流不会撞（候选批次 step_id 必指向 active step）。
                    (String::new(), format!("(deleted#{pid})"))
                });
                ProcessBatchCount {
                    process_id: pid,
                    process_code: code,
                    process_name: name,
                    count,
                }
            })
            .collect();

        Ok(WorkerPoolCountsOut { counts, total })
    }

    /// `POST /api/v2/admin/worker-pool/auto-allocate` 业务逻辑。
    ///
    /// 按 `process_id + shelf_id` 范围，对每个匹配 worker 计算 target 并循环 refill。
    pub async fn auto_allocate_for_process(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: AutoAllocateRequest,
        current: &CurrentUser,
    ) -> Result<AutoAllocateResult, AppError> {
        current.require_role(Role::Manager)?;

        // 1. 校验 fill_ratio
        if !(0.0..=1.0).contains(&req.fill_ratio) {
            return Err(AppError::biz(
                code::BIZ_AUTO_ALLOCATE_INVALID_RATIO,
                format!("fill_ratio 必须在 [0.0, 1.0]，当前 {}", req.fill_ratio),
            ));
        }

        // 2. process 存在性
        let _process = (&mut *conn)
            .process_get_by_id(req.process_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process {} 不存在", req.process_id),
                )
            })?;

        // 3. process 映射的 work_types
        let work_types = (&mut *conn)
            .work_type_list_work_types_by_process_id(req.process_id)
            .await?;
        if work_types.is_empty() {
            return Err(AppError::biz(
                code::BIZ_WORK_TYPE_NO_PROCESS_MAPPING,
                format!("process {} 未映射工种", req.process_id),
            ));
        }
        // work_type_id → (max_held_batches, max_held_minutes)
        let mut wt_max: std::collections::HashMap<i64, (Option<i32>, Option<i32>)> =
            std::collections::HashMap::new();
        for (wt_id, _code, _name, max_held_batches) in work_types {
            let max_minutes = (&mut *conn).work_type_get_max_held_minutes(wt_id).await?;
            wt_max.insert(wt_id, (max_held_batches, max_minutes));
        }

        // 4. 货架上的 active worker 列表（含 work_type_id）
        let worker_rows = (&mut *conn)
            .worker_list_active_by_process_id(req.process_id)
            .await?;

        let mut filled = Vec::with_capacity(worker_rows.len());
        let mut pool_empty_any = false;

        for (worker_id, _worker_name, work_type_id, _wt_code) in worker_rows {
            if work_type_id == 0 {
                filled.push(WorkerFillItem {
                    worker_id,
                    target: 0,
                    filled_count: 0,
                    skipped_reason: Some("worker 无 work_type".to_string()),
                });
                continue;
            }
            let (max_batches, max_minutes) = match wt_max.get(&work_type_id) {
                Some(v) => *v,
                None => continue,
            };

            let worker = match (&mut *conn).worker_get_by_id(worker_id, false).await? {
                Some(w) => w,
                None => continue,
            };
            if !worker.is_active {
                continue;
            }

            let target: i32 = match req.mode {
                AutoAllocateMode::Count => {
                    let max = max_batches.ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_WORK_TYPE_MAX_HELD_NOT_SET,
                            format!(
                                "work_type {} max_held_batches 未设置（COUNT 模式）",
                                work_type_id
                            ),
                        )
                    })?;
                    ((max as f64) * req.fill_ratio).ceil() as i32
                }
                AutoAllocateMode::Time => {
                    let max = max_minutes.ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET,
                            format!(
                                "work_type {} max_held_minutes 未设置（TIME 模式）",
                                work_type_id
                            ),
                        )
                    })?;
                    ((max as f64) * req.fill_ratio).ceil() as i32
                }
            };

            let process_ids = (&mut *conn)
                .work_type_list_process_ids(work_type_id)
                .await?;
            if process_ids.is_empty() {
                continue;
            }

            let mut filled_count = 0i32;
            for _ in 0..target {
                match (&mut *conn)
                    .take_one_from_pool(worker_id, req.shelf_id, &process_ids, current.id)
                    .await?
                {
                    Some(t) => {
                        let event_id = snowflake.next_id();
                        (&mut *conn)
                            .part_insert_part_event(
                                event_id,
                                t.part_id,
                                "TAKEN_FROM_POOL",
                                Some("IN_PROCESS"),
                                Some("IN_PROCESS"),
                                Some(t.batch_id),
                                Some(t.quantity),
                                Some(&t.drawing_no),
                                Some(&worker.badge_code),
                                Some("auto_allocate"),
                                Some(current.id),
                            )
                            .await?;
                        PartService::sync_from_batch_change_with_conn(
                            &mut *conn, t.part_id, current,
                        )
                        .await?;
                        filled_count += 1;
                    }
                    None => {
                        pool_empty_any = true;
                        break;
                    }
                }
            }

            filled.push(WorkerFillItem {
                worker_id,
                target,
                filled_count,
                skipped_reason: None,
            });
        }

        Ok(AutoAllocateResult {
            process_id: req.process_id,
            shelf_id: req.shelf_id,
            mode: req.mode,
            fill_ratio: req.fill_ratio,
            filled,
            pool_empty: pool_empty_any,
        })
    }

    // 2026-09-30 重构：原 `assign_batch_to_worker`（POOL→WORKER 单边端点）已删除，
    // 该功能由通用 `move_batch` 端点（POOL→WORKER 分支）取代。
    // 旧调用点（POST /api/v2/admin/worker-pool/assign）由 router 层移除。
}

impl Default for WorkerPoolService {
    fn default() -> Self {
        Self::new()
    }
}

// ===== in-source 单测 =====
//
// 2026-09-30 review 第 1 轮补漏：plan §5.6 要求 move_batch 的核心方向 + 校验失败
// 路径在 service.rs 末尾 in-source 覆盖。原 plan 第 1 轮实现仅写了集成测试
// （tests/production/worker_pool.rs::move_*_transfers_batch 等），未在 service
// 内做精细单测。本文件补 7 个场景：
// - 三方向 happy path：POOL→WORKER / WORKER→POOL / WORKER→WORKER
// - from 与 batch 实际 (location, holder) 不一致 → 40904 LOCATION_MISMATCH
// - 目标 worker 工种不含 batch 当前工序（current_process_id）→ 20104 BIZ_INVALID_VALUE
// - 目标 shelf 未映射工序 → 20507 BIZ_SHELF_PROCESS_NOT_MAPPED
// - POOL→POOL 同 kind 移动 → 40001 VALIDATION_ERROR
//
// helper 沿用 batch/service.rs::mod tests 风格：直接 raw INSERT，避免引
// repo trait 而膨胀测试体积。
//
// `hsh-erp-test-support` 是 dev-only crate（仅 `[dev-dependencies]` 引入），
// 故 mod tests 必须 `#[cfg(test)]`，否则普通 `cargo check` 会报 unresolved import。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::rbac::Role;
    use crate::infra::clock::now_naive;
    use hsh_erp_test_support::test_pool;

    /// 进程级共享雪花 ID 生成器（与 tests/production/worker_pool.rs 同源设计）。
    /// 多个 in-source test 在同一毫秒内连发 helper，独立构造会拿到相同 id
    /// （23505 pkey 冲突）。
    fn pool_snowflake() -> &'static std::sync::Mutex<SnowflakeIdGenerator> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<SnowflakeIdGenerator>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(SnowflakeIdGenerator::new(1_577_836_800_000, 7)))
    }

    // ===== helper =====

    async fn insert_user_with_role(
        pool: &sqlx::PgPool,
        username: &str,
        plain_password: &str,
        role: &str,
    ) -> i64 {
        use crate::auth::password;
        let hash = password::hash(plain_password).expect("bcrypt");
        // 一次性取两个 id 后立即 drop MutexGuard（避免跨 .await 持锁触发
        // `clippy::await_holding_lock`）。
        let (user_id, role_id) = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            (s.next_id(), s.next_id())
        };
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
             refresh_token_version, version, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
        )
        .bind(user_id)
        .bind(username.to_lowercase())
        .bind(hash)
        .bind(username)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_user");
        sqlx::query(
            "INSERT INTO t_user_role (id, user_id, role, version, created_at, updated_at) \
             VALUES ($1, $2, $3, 0, $4, $4)",
        )
        .bind(role_id)
        .bind(user_id)
        .bind(role)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_user_role");
        user_id
    }

    /// 写一个 INHOUSE 工序。
    async fn insert_process(pool: &sqlx::PgPool, code: &str, name: &str) -> i64 {
        let id = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            s.next_id()
        };
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
             version, created_at, updated_at) \
             VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
        )
        .bind(id)
        .bind(code)
        .bind(name)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_process");
        id
    }

    /// 写一个工种（可指定 max_held_batches）。
    async fn insert_work_type(
        pool: &sqlx::PgPool,
        code: &str,
        name: &str,
        max_held: Option<i32>,
    ) -> i64 {
        let id = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            s.next_id()
        };
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_work_type (id, code, name, sort_order, max_held_batches, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $3, 0, $4, 0, $5, $5)",
        )
        .bind(id)
        .bind(code)
        .bind(name)
        .bind(max_held)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_work_type");
        id
    }

    async fn link_work_type_to_process(pool: &sqlx::PgPool, wt_id: i64, p_id: i64) {
        let id = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            s.next_id()
        };
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, version, \
             created_at, updated_at) VALUES ($1, $2, $3, 0, 0, $4, $4)",
        )
        .bind(id)
        .bind(wt_id)
        .bind(p_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_work_type_process");
    }

    /// 写一个 PRODUCTION 货架。
    async fn insert_shelf(pool: &sqlx::PgPool, code: &str, zone: &str) -> i64 {
        let id = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            s.next_id()
        };
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $2, $3, true, 0, 0, $4, $4)",
        )
        .bind(id)
        .bind(code)
        .bind(zone)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_shelf");
        id
    }

    async fn link_shelf_to_process(pool: &sqlx::PgPool, shelf_id: i64, process_id: i64) {
        let id = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            s.next_id()
        };
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
             created_at, updated_at) VALUES ($1, $2, $3, 0, 0, $4, $4)",
        )
        .bind(id)
        .bind(shelf_id)
        .bind(process_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_shelf_process");
    }

    /// 写一个 worker（active）。
    async fn insert_worker(
        pool: &sqlx::PgPool,
        badge_code: &str,
        work_type_id: Option<i64>,
    ) -> i64 {
        let id = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            s.next_id()
        };
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $2, true, $3, 0, $4, $4)",
        )
        .bind(id)
        .bind(badge_code)
        .bind(work_type_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_worker");
        id
    }

    /// 写一个 L2 customer（parent_id NULL 表示 L1 叶子）。
    async fn insert_customer(pool: &sqlx::PgPool, name: &str) -> i64 {
        let id = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            s.next_id()
        };
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_customer (id, name, version, created_at, updated_at) \
             VALUES ($1, $2, 0, $3, $3)",
        )
        .bind(id)
        .bind(name)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_customer");
        id
    }

    /// 写一个 part + chain + step（首道指向 process_id）。返回 part_id。
    /// 2026-09-16 PR-3：move 路径要求 part 绑定工艺链 + batch 持有
    /// current_process_step_id（与 worker_pool 集成测试 helper 同形态）。
    /// 2026-09-30：候选池归属改按 `current_process_id` 过滤，helper 同步补该列。
    async fn insert_pool_batch(
        pool: &sqlx::PgPool,
        customer_id: i64,
        process_id: i64,
        shelf_id: i64,
    ) -> (i64, i64) {
        let now = now_naive();
        let today = now.date();
        // 一次性取 4 个 id（chain / step / part / batch），drop guard 后再 await。
        let (chain_id, step_id, part_id, batch_id) = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            (s.next_id(), s.next_id(), s.next_id(), s.next_id())
        };

        sqlx::query(
            "INSERT INTO t_part_process_chain (id, version, created_at, created_by, updated_at, \
             updated_by) VALUES ($1, 0, $2, 1, $2, 1)",
        )
        .bind(chain_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert chain");

        sqlx::query(
            "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
             estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
             VALUES ($1, $2, 1, $3, 30, 0, $4, 0, $4, 0)",
        )
        .bind(step_id)
        .bind(chain_id)
        .bind(process_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert chain step");

        sqlx::query(
            "INSERT INTO t_part (id, name, drawing_no, applicant_name, request_date, \
             planned_delivery_date, status, customer_id, quantity, version, created_at, \
             updated_at, process_chain_id) \
             VALUES ($1, 'pool-item', 'DWG-POOL', '', $2, $2, 'IN_PROCESS', $3, 1, 0, $4, $4, $5)",
        )
        .bind(part_id)
        .bind(today)
        .bind(customer_id)
        .bind(now)
        .bind(chain_id)
        .execute(pool)
        .await
        .expect("insert t_part");

        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, current_process_id, current_process_step_id, version, \
             created_at, updated_at) \
             VALUES ($1, $2, 1, 1, 'IN_PROCESS', 'PRODUCTION_SHELF', $3, $4, $5, 0, $6, $6)",
        )
        .bind(batch_id)
        .bind(part_id)
        .bind(shelf_id)
        .bind(process_id)
        .bind(step_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_part_batch POOL");
        (part_id, batch_id)
    }

    /// 写一个 part + chain + step + IN_PROCESS+WORKER 批次（被 worker 持有）。
    async fn insert_worker_held_batch(
        pool: &sqlx::PgPool,
        customer_id: i64,
        process_id: i64,
        worker_id: i64,
    ) -> (i64, i64) {
        let now = now_naive();
        let today = now.date();
        let (chain_id, step_id, part_id, batch_id) = {
            let s = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
            (s.next_id(), s.next_id(), s.next_id(), s.next_id())
        };

        sqlx::query(
            "INSERT INTO t_part_process_chain (id, version, created_at, created_by, updated_at, \
             updated_by) VALUES ($1, 0, $2, 1, $2, 1)",
        )
        .bind(chain_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert chain");

        sqlx::query(
            "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
             estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
             VALUES ($1, $2, 1, $3, 30, 0, $4, 0, $4, 0)",
        )
        .bind(step_id)
        .bind(chain_id)
        .bind(process_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert chain step");

        sqlx::query(
            "INSERT INTO t_part (id, name, drawing_no, applicant_name, request_date, \
             planned_delivery_date, status, customer_id, quantity, version, created_at, \
             updated_at, process_chain_id) \
             VALUES ($1, 'held-item', 'DWG-HELD', '', $2, $2, 'IN_PROCESS', $3, 1, 0, $4, $4, $5)",
        )
        .bind(part_id)
        .bind(today)
        .bind(customer_id)
        .bind(now)
        .bind(chain_id)
        .execute(pool)
        .await
        .expect("insert t_part");

        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, current_process_id, current_process_step_id, version, \
             created_at, updated_at) \
             VALUES ($1, $2, 1, 1, 'IN_PROCESS', 'WORKER', $3, $4, $5, 0, $6, $6)",
        )
        .bind(batch_id)
        .bind(part_id)
        .bind(worker_id)
        .bind(process_id)
        .bind(step_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_part_batch WORKER");
        (part_id, batch_id)
    }

    fn make_current(user_id: i64, role: Role) -> CurrentUser {
        CurrentUser {
            id: user_id,
            username: "test".to_string(),
            roles: vec![role],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    // ===== 测试 =====

    #[tokio::test]
    async fn move_batch_pool_to_worker_succeeds() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager_pool", "password", "MANAGER").await;
        let customer = insert_customer(&pool, "ACME-PTW").await;
        let proc = insert_process(&pool, "PROC-PTW", "工序PTW").await;
        let wt = insert_work_type(&pool, "WT-PTW", "工种PTW", Some(3)).await;
        link_work_type_to_process(&pool, wt, proc).await;
        let prod_shelf = insert_shelf(&pool, "SH-PTW", "PRODUCTION").await;
        link_shelf_to_process(&pool, prod_shelf, proc).await;
        let worker = insert_worker(&pool, "BC-PTW", Some(wt)).await;
        let (_part, batch) = insert_pool_batch(&pool, customer, proc, prod_shelf).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = pool_snowflake()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_id();
        let snowflake_obj = SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let _ = snowflake; // 占位避免 unused warning
        let req = MoveRequest {
            batch_id: batch,
            from: MoveLocation::Pool {
                shelf_id: prod_shelf,
            },
            to: MoveLocation::Worker { worker_id: worker },
            note: Some("in-source pool→worker".to_string()),
        };
        let r = WorkerPoolService::move_batch(
            &mut conn,
            &snowflake_obj,
            req,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("move_batch OK");
        assert_eq!(r.from_kind, "POOL");
        assert_eq!(r.to_kind, "WORKER");
        assert_eq!(r.new_holder_id, worker);
        assert_eq!(r.new_location, "WORKER");
        assert_eq!(r.current_held, Some(1));
        assert_eq!(r.max_held, Some(3));
        assert_eq!(r.shelf_id, Some(prod_shelf));

        // DB 验证：batch 已切到 WORKER + holder=worker；step 不变
        let row: (String, Option<i64>) =
            sqlx::query_as("SELECT location, current_holder_id FROM t_part_batch WHERE id = $1")
                .bind(batch)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "WORKER");
        assert_eq!(row.1, Some(worker));
    }

    #[tokio::test]
    async fn move_batch_worker_to_pool_succeeds() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager_wtp", "password", "MANAGER").await;
        let customer = insert_customer(&pool, "ACME-WTP").await;
        let proc = insert_process(&pool, "PROC-WTP", "工序WTP").await;
        let wt = insert_work_type(&pool, "WT-WTP", "工种WTP", Some(3)).await;
        link_work_type_to_process(&pool, wt, proc).await;
        let prod_shelf = insert_shelf(&pool, "SH-WTP", "PRODUCTION").await;
        link_shelf_to_process(&pool, prod_shelf, proc).await;
        let worker = insert_worker(&pool, "BC-WTP", Some(wt)).await;
        let (_part, batch) = insert_worker_held_batch(&pool, customer, proc, worker).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake_obj = SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let req = MoveRequest {
            batch_id: batch,
            from: MoveLocation::Worker { worker_id: worker },
            to: MoveLocation::Pool {
                shelf_id: prod_shelf,
            },
            note: Some("in-source worker→pool".to_string()),
        };
        let r = WorkerPoolService::move_batch(
            &mut conn,
            &snowflake_obj,
            req,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("move_batch OK");
        assert_eq!(r.from_kind, "WORKER");
        assert_eq!(r.to_kind, "POOL");
        assert_eq!(r.new_holder_id, prod_shelf);
        assert_eq!(r.new_location, "PRODUCTION_SHELF");
        assert_eq!(r.shelf_id, Some(prod_shelf));

        // DB 验证：batch 回到 PRODUCTION_SHELF + holder=shelf
        let row: (String, Option<i64>) =
            sqlx::query_as("SELECT location, current_holder_id FROM t_part_batch WHERE id = $1")
                .bind(batch)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "PRODUCTION_SHELF");
        assert_eq!(row.1, Some(prod_shelf));
    }

    #[tokio::test]
    async fn move_batch_worker_to_worker_succeeds() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager_wtw", "password", "MANAGER").await;
        let customer = insert_customer(&pool, "ACME-WTW").await;
        let proc = insert_process(&pool, "PROC-WTW", "工序WTW").await;
        let wt = insert_work_type(&pool, "WT-WTW", "工种WTW", Some(3)).await;
        link_work_type_to_process(&pool, wt, proc).await;
        let prod_shelf = insert_shelf(&pool, "SH-WTW", "PRODUCTION").await;
        link_shelf_to_process(&pool, prod_shelf, proc).await;
        let worker_src = insert_worker(&pool, "BC-WTW-SRC", Some(wt)).await;
        let worker_dst = insert_worker(&pool, "BC-WTW-DST", Some(wt)).await;
        let (_part, batch) = insert_worker_held_batch(&pool, customer, proc, worker_src).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake_obj = SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let req = MoveRequest {
            batch_id: batch,
            from: MoveLocation::Worker {
                worker_id: worker_src,
            },
            to: MoveLocation::Worker {
                worker_id: worker_dst,
            },
            note: Some("in-source worker→worker".to_string()),
        };
        let r = WorkerPoolService::move_batch(
            &mut conn,
            &snowflake_obj,
            req,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("move_batch OK");
        assert_eq!(r.from_kind, "WORKER");
        assert_eq!(r.to_kind, "WORKER");
        assert_eq!(r.new_holder_id, worker_dst);
        assert_eq!(r.new_location, "WORKER");
        assert_eq!(r.current_held, Some(1));
        assert_eq!(r.max_held, Some(3));

        // DB 验证：batch 切到 worker_dst
        let row: Option<i64> =
            sqlx::query_scalar("SELECT current_holder_id FROM t_part_batch WHERE id = $1")
                .bind(batch)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, Some(worker_dst));
    }

    /// 场景：from.kind=WORKER 但 batch 实际在 POOL → 40904 LOCATION_MISMATCH。
    #[tokio::test]
    async fn move_batch_from_mismatch_returns_40904() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager_mm", "password", "MANAGER").await;
        let customer = insert_customer(&pool, "ACME-MM").await;
        let proc = insert_process(&pool, "PROC-MM", "工序MM").await;
        let wt = insert_work_type(&pool, "WT-MM", "工种MM", Some(3)).await;
        link_work_type_to_process(&pool, wt, proc).await;
        let prod_shelf = insert_shelf(&pool, "SH-MM", "PRODUCTION").await;
        link_shelf_to_process(&pool, prod_shelf, proc).await;
        let (_part, batch) = insert_pool_batch(&pool, customer, proc, prod_shelf).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake_obj = SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let req = MoveRequest {
            batch_id: batch,
            // from 谎报成 WORKER（实际在 POOL），期望 40904
            from: MoveLocation::Worker {
                worker_id: 999_999_999,
            },
            to: MoveLocation::Pool {
                shelf_id: prod_shelf,
            },
            note: None,
        };
        let err = WorkerPoolService::move_batch(
            &mut conn,
            &snowflake_obj,
            req,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("from 不匹配应 Err");
        assert_eq!(err.code(), code::BIZ_BATCH_LOCATION_MISMATCH);
    }

    /// 场景：目标 worker 工种不含 batch 当前工序（current_process_id）→ 20104 BIZ_INVALID_VALUE。
    #[tokio::test]
    async fn move_batch_target_worker_ineligible_returns_biz_invalid_value() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager_tw", "password", "MANAGER").await;
        let customer = insert_customer(&pool, "ACME-TW").await;
        // batch 当前工序 = PROC-X（同时写 current_process_id 与 step）
        let proc_x = insert_process(&pool, "PROC-X", "工序X").await;
        let wt_src = insert_work_type(&pool, "WT-X", "工种X", Some(5)).await;
        link_work_type_to_process(&pool, wt_src, proc_x).await;
        let prod_shelf = insert_shelf(&pool, "SH-X", "PRODUCTION").await;
        link_shelf_to_process(&pool, prod_shelf, proc_x).await;
        let worker_src = insert_worker(&pool, "BC-X-SRC", Some(wt_src)).await;
        let (_part, batch) = insert_worker_held_batch(&pool, customer, proc_x, worker_src).await;

        // 目标 worker 工种仅含 PROC-Y（不含 PROC-X）
        let proc_y = insert_process(&pool, "PROC-Y", "工序Y").await;
        let wt_dst = insert_work_type(&pool, "WT-Y", "工种Y", Some(5)).await;
        link_work_type_to_process(&pool, wt_dst, proc_y).await;
        let worker_dst = insert_worker(&pool, "BC-X-DST", Some(wt_dst)).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake_obj = SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let req = MoveRequest {
            batch_id: batch,
            from: MoveLocation::Worker {
                worker_id: worker_src,
            },
            to: MoveLocation::Worker {
                worker_id: worker_dst,
            },
            note: None,
        };
        let err = WorkerPoolService::move_batch(
            &mut conn,
            &snowflake_obj,
            req,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("工序不合格应 Err");
        assert_eq!(err.code(), code::BIZ_INVALID_VALUE);
    }

    /// 场景：目标 shelf 未映射 batch 当前工序（current_process_id）→ 20507 SHELF_PROCESS_NOT_MAPPED。
    /// 走 WORKER→POOL 分支（to=POOL 时会校验 shelf 映射）。
    #[tokio::test]
    async fn move_batch_target_shelf_unmapped_returns_20507() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager_ts", "password", "MANAGER").await;
        let customer = insert_customer(&pool, "ACME-TS").await;
        // batch 当前工序 = PROC-P
        let proc_p = insert_process(&pool, "PROC-P", "工序P").await;
        let wt = insert_work_type(&pool, "WT-P", "工种P", Some(5)).await;
        link_work_type_to_process(&pool, wt, proc_p).await;
        // 源 shelf 映射 PROC-P
        let src_shelf = insert_shelf(&pool, "SH-P-SRC", "PRODUCTION").await;
        link_shelf_to_process(&pool, src_shelf, proc_p).await;
        // 目标 shelf 映射另一个 PROC-Q（不映射 PROC-P）
        let proc_q = insert_process(&pool, "PROC-Q", "工序Q").await;
        let dst_shelf = insert_shelf(&pool, "SH-Q-DST", "PRODUCTION").await;
        link_shelf_to_process(&pool, dst_shelf, proc_q).await;

        let worker = insert_worker(&pool, "BC-P", Some(wt)).await;
        let (_part, batch) = insert_worker_held_batch(&pool, customer, proc_p, worker).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake_obj = SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let req = MoveRequest {
            batch_id: batch,
            from: MoveLocation::Worker { worker_id: worker },
            to: MoveLocation::Pool {
                shelf_id: dst_shelf,
            },
            note: None,
        };
        let err = WorkerPoolService::move_batch(
            &mut conn,
            &snowflake_obj,
            req,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("shelf 未映射应 Err");
        assert_eq!(err.code(), code::BIZ_SHELF_PROCESS_NOT_MAPPED);
    }

    /// 场景：POOL→POOL 同 kind 移动 → 40001 VALIDATION_ERROR。
    #[tokio::test]
    async fn move_batch_same_kind_returns_validation_error() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager_sk", "password", "MANAGER").await;
        let customer = insert_customer(&pool, "ACME-SK").await;
        let proc = insert_process(&pool, "PROC-SK", "工序SK").await;
        let prod_shelf = insert_shelf(&pool, "SH-SK", "PRODUCTION").await;
        link_shelf_to_process(&pool, prod_shelf, proc).await;
        let (_part, batch) = insert_pool_batch(&pool, customer, proc, prod_shelf).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake_obj = SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let req = MoveRequest {
            batch_id: batch,
            from: MoveLocation::Pool {
                shelf_id: prod_shelf,
            },
            to: MoveLocation::Pool {
                shelf_id: prod_shelf,
            },
            note: None,
        };
        let err = WorkerPoolService::move_batch(
            &mut conn,
            &snowflake_obj,
            req,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("同 kind 应 Err");
        // 40001 VALIDATION_ERROR 在 AppError::validation 路径生成
        assert_eq!(err.code(), 40001);
    }
}
