//! worker_pool 域业务逻辑
//!
//! 对应 Python myERP/service/worker_pool_service.py。
//!
//! ## 阶段 worker-pool-take（Task 7）
//! - `refill_for_worker` —— admin 触发「为某 worker 从其工序池抢满 max_held_batches」循环；
//!   内部循环调 `WorkerPoolRepoTrait::take_one_from_pool`，直到池空或达到上限；
//!   每抢到一批写一条 `TAKEN_FROM_POOL` 事件日志（commit 由 handler 负责）。
//! - `compute_state` —— worker 当前持有数 + 池候选数（按工序分组）；用于 state 端点。
//! - `admin_remove_held_batch` —— admin 主动把 worker 持有的某批次按 RETURNED 语义放回
//!   候选池；调 `part_mark_batch_returned`（OCC）+ `sync_from_batch_change`
//!   同步 part 派生列 + 写事件日志。
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

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::service::PartService;
use crate::modules::prod::worker_pool::repo::WorkerPoolRepoTrait;
use crate::shared::error::{AppError, code};

use super::dto::{
    AdminAssignRequest, AdminRemoveRequest, AssignResult, AutoAllocateMode, AutoAllocateRequest,
    AutoAllocateResult, PoolBatchItem, ProcessPoolDetail, WorkTypeMaxHeld, WorkerBrief,
    WorkerFillItem,
};
use super::model::{ProcessPoolCount, RefillResult, TakenItem, WorkerPoolState};

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
        let worker = (&mut *conn).worker_get_by_id( worker_id, false)
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
        let work_type = (&mut *conn).work_type_get_by_id( work_type_id)
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

        let process_ids =
            (&mut *conn).work_type_list_process_ids( work_type_id).await?;
        if process_ids.is_empty() {
            return Err(AppError::biz(
                code::BIZ_WORK_TYPE_NO_PROCESS_MAPPING,
                format!("work_type {work_type_id} 未映射工序"),
            ));
        }

        let mut taken = Vec::new();
        while let Some(t) = (&mut *conn).take_one_from_pool(
            worker_id,
            shelf_id,
            &process_ids,
            operator_user_id,
        )
        .await?
        {
            // part_event.id 用 snowflake 生成（与既有 `PartRepo::insert_part_event` 调用约定一致）
            let event_id = snowflake.next_id();
            (&mut *conn).part_insert_part_event(
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
        let worker = (&mut *conn).worker_get_by_id( worker_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_WORKER_NOT_FOUND, "worker 不存在"))?;
        let work_type = if let Some(wt_id) = worker.work_type_id {
            (&mut *conn).work_type_get_by_id( wt_id).await?
        } else {
            None
        };
        let max_held = work_type
            .as_ref()
            .and_then(|w| w.max_held_batches)
            .unwrap_or(0);
        let current_held =
            (&mut *conn).part_batch_count_held_by_worker( worker_id).await?;
        let capacity_remaining = (max_held as i64 - current_held).max(0) as i32;

        let process_ids = if let Some(wt_id) = worker.work_type_id {
            (&mut *conn).work_type_list_process_ids( wt_id).await?
        } else {
            vec![]
        };

        let mut pool_count_by_process = Vec::with_capacity(process_ids.len());
        for pid in &process_ids {
            let n = (&mut *conn).count_pool_by_shelf_and_process( shelf_id, *pid)
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
        let held_batches =
            (&mut *conn).list_held_by_worker_with_part( worker_id).await?;

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

    /// admin 主动把 worker 持有的某批次按 RETURNED 语义放回候选池。
    ///
    /// 流程：
    /// 1. 取 worker（事件日志 `badge_code` 需要）；
    /// 2. 按 `(batch_id, holder_id = worker_id)` 找 IN_PROCESS+WORKER 批次，
    ///    找不到 → `20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER`；
    /// 3. `mark_batch_returned`（OCC：version 冲突 →
    ///    `40901`）；shelf+next_process 由 admin 在 req 里指定（不校验 shelf
    ///    是否映射该 process —— 若 shelf 不映射此 process，下一次 worker refill
    ///    自然拿不到，由 service 抛出业务错时再处理）；
    /// 4. 写 `ADMIN_REMOVED_FROM_WORKER` 事件日志；
    /// 5. 返回 `TakenItem`（`version = batch.version + 1`，与其它 mark_* 流一致）。
    pub async fn admin_remove_held_batch(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: AdminRemoveRequest,
        current: &CurrentUser,
    ) -> Result<TakenItem, AppError> {
        // 1. 取 worker（带 work_type）
        let worker = (&mut *conn).worker_get_by_id( req.worker_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_WORKER_NOT_FOUND, "worker 不存在"))?;
        // 2. 找 batch（必须是该 worker 持有）
        let batch = (&mut *conn).part_find_inprocess_batch_by_id_and_holder(
            req.batch_id,
            req.worker_id,
        )
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_HELD_BY_WORKER,
                format!("batch {} 不是 worker {} 持有", req.batch_id, req.worker_id),
            )
        })?;
        // 3. 切 holder 到 shelf + 改 current_process_step_id（OCC，batch 级）
        //    PR-3 批次 step 化：admin_remove 路径下 step_id 由 caller
        //    （前端 admin UI）解析或由 service 兜底；这里先按 process_id
        //    查 chain step（admin_remove 前要求 part 已绑定链）
        let chain_id_opt =
            (&mut *conn).part_get_process_chain_id( batch.part_id).await?;
        let step_id_opt = if let Some(chain_id) = chain_id_opt {
            (&mut *conn).process_chain_resolve_step_id_by_process(
                chain_id,
                req.next_process_id,
            )
            .await?
        } else {
            None
        };
        let batch_rows = (&mut *conn).part_mark_batch_returned(
            batch.id,
            batch.version,
            req.shelf_id,
            step_id_opt,
            Some(current.id),
        )
        .await?;
        if batch_rows == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("batch {} 版本冲突或状态非 IN_PROCESS+WORKER", batch.id),
            ));
        }
        let part = (&mut *conn).part_get_by_id( batch.part_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        // 4. PR-B2：part 派生列由 sync_from_batch_change 统一回填
        //    （location=PRODUCTION_SHELF / holder=shelf / next_process）。
        PartService::sync_from_batch_change_with_conn(&mut *conn, part.id, current).await?;
        // 5. event
        let event_id = snowflake.next_id();
        (&mut *conn).part_insert_part_event(
            event_id,
            part.id,
            "ADMIN_REMOVED_FROM_WORKER",
            Some("IN_PROCESS"),
            Some("IN_PROCESS"),
            Some(batch.id),
            Some(batch.quantity),
            Some(&part.drawing_no),
            Some(&worker.badge_code),
            Some("admin_remove"),
            Some(current.id),
        )
        .await?;
        // 6. 返回
        Ok(TakenItem {
            batch_id: batch.id,
            part_id: part.id,
            batch_no: batch.batch_no,
            quantity: batch.quantity,
            serial_no: part.serial_no,
            drawing_no: part.drawing_no,
            system_delivery_date: part.system_delivery_date,
            planned_delivery_date: Some(part.planned_delivery_date),
            is_urgent: part.is_urgent,
            version: batch.version + 1,
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
        let process = (&mut *conn).process_get_by_id( process_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process {process_id} 不存在"),
                )
            })?;

        // 2. work_types
        let work_types =
            (&mut *conn).work_type_list_work_types_by_process_id( process_id)
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
        let worker_rows =
            (&mut *conn).worker_list_active_by_process_id( process_id).await?;
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
        let items: Vec<PoolBatchItem> =
            (&mut *conn).list_candidates_by_process_all_shelves( process_id)
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
        let _process = (&mut *conn).process_get_by_id( req.process_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process {} 不存在", req.process_id),
                )
            })?;

        // 3. process 映射的 work_types
        let work_types =
            (&mut *conn).work_type_list_work_types_by_process_id( req.process_id)
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
            let max_minutes =
                (&mut *conn).work_type_get_max_held_minutes( wt_id).await?;
            wt_max.insert(wt_id, (max_held_batches, max_minutes));
        }

        // 4. 货架上的 active worker 列表（含 work_type_id）
        let worker_rows =
            (&mut *conn).worker_list_active_by_process_id( req.process_id)
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

            let worker = match (&mut *conn).worker_get_by_id( worker_id, false)
                .await?
            {
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

            let process_ids =
                (&mut *conn).work_type_list_process_ids( work_type_id).await?;
            if process_ids.is_empty() {
                continue;
            }

            let mut filled_count = 0i32;
            for _ in 0..target {
                match (&mut *conn).take_one_from_pool(
                    worker_id,
                    req.shelf_id,
                    &process_ids,
                    current.id,
                )
                .await?
                {
                    Some(t) => {
                        let event_id = snowflake.next_id();
                        (&mut *conn).part_insert_part_event(
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
                        PartService::sync_from_batch_change_with_conn(&mut *conn, t.part_id, current).await?;
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

    /// `POST /api/v2/admin/worker-pool/assign` 业务逻辑（单 batch 分配）。
    pub async fn assign_batch_to_worker(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: AdminAssignRequest,
        current: &CurrentUser,
    ) -> Result<AssignResult, AppError> {
        current.require_role(Role::Manager)?;

        let worker = (&mut *conn).worker_get_by_id( req.worker_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORKER_NOT_FOUND,
                    format!("worker {} 不存在", req.worker_id),
                )
            })?;
        if !worker.is_active {
            return Err(AppError::biz(
                code::BIZ_WORKER_INACTIVE,
                format!("worker {} 已停用", req.worker_id),
            ));
        }
        let work_type_id = worker.work_type_id.ok_or_else(|| {
            AppError::biz(
                code::BIZ_WORKER_NO_WORK_TYPE,
                format!("worker {} 未分配工种", req.worker_id),
            )
        })?;
        let work_type = (&mut *conn).work_type_get_by_id( work_type_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORK_TYPE_NOT_FOUND,
                    format!("work_type {work_type_id} 不存在"),
                )
            })?;
        let max_held = work_type.max_held_batches.ok_or_else(|| {
            AppError::biz(
                code::BIZ_WORK_TYPE_MAX_HELD_NOT_SET,
                format!("work_type {work_type_id} max_held_batches 未设置"),
            )
        })?;

        let current_held_before = (&mut *conn).part_batch_count_held_by_worker(
            req.worker_id,
        )
        .await?;
        if current_held_before >= max_held as i64 {
            return Err(AppError::biz(
                code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED,
                format!(
                    "worker {} 已持有 {} 批次，工种上限 {} 触顶",
                    req.worker_id, current_held_before, max_held
                ),
            ));
        }

        if let Some(pid) = req.process_id {
            let batch =
                (&mut *conn).part_batch_get_by_id( req.batch_id, false)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_PART_BATCH_NOT_FOUND,
                            format!("batch {} 不存在", req.batch_id),
                        )
                    })?;
            let step_process_id = if let Some(step_id) = batch.current_process_step_id {
                (&mut *conn).process_chain_step_get_process_id( step_id).await?
            } else {
                None
            };
            match step_process_id {
                Some(spid) if spid == pid => {}
                _ => {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        format!(
                            "batch {} 当前 step.process_id={:?} 与 request process_id={} 不匹配",
                            req.batch_id, step_process_id, pid
                        ),
                    ));
                }
            }
        }

        let taken = (&mut *conn).take_specific_from_pool(
            req.worker_id,
            req.shelf_id,
            req.batch_id,
            current.id,
        )
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_HELD_BY_WORKER,
                format!(
                    "batch {} 不在候选池（status/location/holder 不符或已软删）",
                    req.batch_id
                ),
            )
        })?;

        PartService::sync_from_batch_change_with_conn(&mut *conn, taken.part_id, current).await?;

        let event_id = snowflake.next_id();
        (&mut *conn).part_insert_part_event(
            event_id,
            taken.part_id,
            "TAKEN_FROM_POOL",
            Some("IN_PROCESS"),
            Some("IN_PROCESS"),
            Some(taken.batch_id),
            Some(taken.quantity),
            Some(&taken.drawing_no),
            Some(&worker.badge_code),
            Some("admin_assign"),
            Some(current.id),
        )
        .await?;

        let current_held_after = current_held_before + 1;
        Ok(AssignResult {
            worker_id: req.worker_id,
            batch_id: req.batch_id,
            shelf_id: req.shelf_id,
            taken,
            current_held: current_held_after as i32,
            max_held,
        })
    }
}

impl Default for WorkerPoolService {
    fn default() -> Self {
        Self::new()
    }
}
