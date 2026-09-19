//! worker_pool 域业务逻辑
//!
//! 对应 Python myERP/service/worker_pool_service.py。
//!
//! ## 阶段 worker-pool-take（Task 7）
//! - `refill_for_worker` —— admin 触发「为某 worker 从其工序池抢满 max_held_batches」循环；
//!   内部循环调 `WorkerPoolRepo::take_one_from_pool`，直到池空或达到上限；
//!   每抢到一批写一条 `TAKEN_FROM_POOL` 事件日志（commit 由 handler 负责）。
//! - `compute_state` —— worker 当前持有数 + 池候选数（按工序分组）；用于 state 端点。
//! - `admin_remove_held_batch` —— admin 主动把 worker 持有的某批次按 RETURNED 语义放回
//!   候选池；调 `PartRepo::mark_batch_returned`（OCC）+ `sync_from_batch_change`
//!   同步 part 派生列 + 写事件日志。
//!
//! ## 阶段 worker-pool-by-process（Task 3）
//! - `pool_by_process` —— admin 按工序查看候选池：process 元数据 + 映射工种 + 可执行工人 +
//!   所有货架候选批次（4 子查询合一，纯读，4 路 `&mut *conn` 复用同一事务）。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepo;
use crate::modules::part::service::PartService;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::modules::process::repo::ProcessRepo;
use crate::modules::process_chain::repo::ProcessChainRepo;
use crate::modules::work_type::repo::WorkTypeRepo;
use crate::modules::worker::repo::WorkerRepo;
use crate::shared::error::{AppError, code};

use super::dto::{
    AdminAssignRequest, AdminRemoveRequest, AssignResult, AutoAllocateMode, AutoAllocateRequest,
    AutoAllocateResult, PoolBatchItem, ProcessPoolDetail, WorkTypeMaxHeld, WorkerBrief,
    WorkerFillItem,
};
use super::model::{ProcessPoolCount, RefillResult, TakenItem, WorkerPoolState};
use super::repo::WorkerPoolRepo;

pub struct WorkerPoolService;

impl WorkerPoolService {
    /// 为 worker 从其货架候选池抢满 `max_held_batches`。
    ///
    /// 流程：
    /// 1. 取 worker（带 work_type_id），校验 `is_active`；
    /// 2. 取 work_type（必须设置 `max_held_batches`）；
    /// 3. 取工种可加工工序 id 列表（process_ids），空 → 业务错
    ///    `BIZ_WORK_TYPE_NO_PROCESS_MAPPING`；
    /// 4. 循环调 `WorkerPoolRepo::take_one_from_pool`，每抢到一批写 `TAKEN_FROM_POOL`
    ///    事件日志（事务内由 handler commit）；
    /// 5. 返回 `RefillResult { worker_id, shelf_id, taken, pool_empty }`。
    ///
    /// 池空 / 容量触顶都会让 `take_one_from_pool` 返回 `Ok(None)`，本方法在
    /// `None` 时跳出循环（业务层不区分二者，由前端按 `pool_empty + taken.len()`
    /// 判断 UI 反馈）。
    #[allow(clippy::too_many_arguments)]
    pub async fn refill_for_worker(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        worker_id: i64,
        shelf_id: i64,
        operator_user_id: i64,
        current: &CurrentUser,
    ) -> Result<RefillResult, AppError> {
        let worker = WorkerRepo::get_by_id(&mut *conn, worker_id, false)
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
    /// 直接接受 `work_type_id` + `badge_code`，跳过 `WorkerRepo::get_by_id` 重复查询。
    ///
    /// 调用方必须保证：
    /// - `worker_id` 已存在
    /// - `worker.is_active == true`
    /// - `worker.work_type_id == Some(work_type_id)`
    ///
    /// 由 [`refill_for_worker`]（admin 路径：自己 fetch）与
    /// [`crate::modules::part::service::PartService::worker_scan_event`]
    /// （worker-scan 路径：service 已在 scan 步骤 fetch 过 worker）共用。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn refill_for_worker_with_work_type(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        worker_id: i64,
        work_type_id: i64,
        shelf_id: i64,
        badge_code: &str,
        operator_user_id: i64,
        current: &CurrentUser,
    ) -> Result<RefillResult, AppError> {
        let work_type = WorkTypeRepo::get_by_id(&mut *conn, work_type_id)
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

        let process_ids = WorkTypeRepo::list_process_ids(&mut *conn, work_type_id).await?;
        if process_ids.is_empty() {
            return Err(AppError::biz(
                code::BIZ_WORK_TYPE_NO_PROCESS_MAPPING,
                format!("work_type {work_type_id} 未映射工序"),
            ));
        }

        let mut taken = Vec::new();
        while let Some(t) = WorkerPoolRepo::take_one_from_pool(
            &mut *conn,
            worker_id,
            shelf_id,
            &process_ids,
            operator_user_id,
        )
        .await?
        {
            PartRepo::insert_part_event(
                &mut *conn,
                NewPartEvent {
                    id: snowflake.next_id(),
                    part_id: t.part_id,
                    event_type: "TAKEN_FROM_POOL",
                    from_status: Some("IN_PROCESS"),
                    to_status: Some("IN_PROCESS"),
                    batch_id: Some(t.batch_id),
                    quantity: Some(t.quantity),
                    drawing_code: Some(&t.drawing_no),
                    badge_code: Some(badge_code),
                    note: None,
                    created_by: Some(operator_user_id),
                },
            )
            .await?;
            // PR-B2：part 派生列（location/holder）由 sync_from_batch_change 统一
            // 回填（worker_id → worker holder，location → 'WORKER'）。
            PartService::sync_from_batch_change(&mut *conn, t.part_id, current).await?;
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
    /// 该 SQL 走 `state` 端点专用，没单独抽 repo（仅一处用）。
    ///
    /// worker 无 work_type 时 `max_held = 0`、`process_ids = []`（state 端点不会拒绝，
    /// 仅展示空池 + 0 上限）。
    pub async fn compute_state(
        conn: &mut PgConnection,
        worker_id: i64,
        shelf_id: i64,
    ) -> Result<WorkerPoolState, AppError> {
        let worker = WorkerRepo::get_by_id(&mut *conn, worker_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_WORKER_NOT_FOUND, "worker 不存在"))?;
        let work_type = if let Some(wt_id) = worker.work_type_id {
            WorkTypeRepo::get_by_id(&mut *conn, wt_id).await?
        } else {
            None
        };
        let max_held = work_type
            .as_ref()
            .and_then(|w| w.max_held_batches)
            .unwrap_or(0);
        let current_held = PartBatchRepo::count_held_by_worker(&mut *conn, worker_id).await?;
        let capacity_remaining = (max_held as i64 - current_held).max(0) as i32;

        let process_ids = if let Some(wt_id) = worker.work_type_id {
            WorkTypeRepo::list_process_ids(&mut *conn, wt_id).await?
        } else {
            vec![]
        };

        let mut pool_count_by_process = Vec::with_capacity(process_ids.len());
        for pid in &process_ids {
            // PR-3 批次 step 化：next_process_id 列已删，JOIN step 取 process_id
            let n: i64 = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "n!" FROM t_part_batch pb
                JOIN t_process_chain_step s ON s.id = pb.current_process_step_id
                WHERE pb.status = 'IN_PROCESS'
                  AND pb.location = 'PRODUCTION_SHELF'
                  AND pb.current_holder_id = $1
                  AND s.process_id = $2
                  AND pb.deleted_at IS NULL
                  AND s.deleted_at IS NULL"#,
                shelf_id,
                pid
            )
            .fetch_one(&mut *conn)
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
            WorkerPoolRepo::list_held_by_worker_with_part(&mut *conn, worker_id).await?;

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
        let worker = WorkerRepo::get_by_id(&mut *conn, req.worker_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_WORKER_NOT_FOUND, "worker 不存在"))?;
        // 2. 找 batch（必须是该 worker 持有）
        let batch = PartRepo::find_inprocess_batch_by_id_and_holder(
            &mut *conn,
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
        let chain_id_opt: Option<i64> = sqlx::query_scalar(
            "SELECT process_chain_id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(batch.part_id)
        .fetch_optional(&mut *conn)
        .await?;
        let step_id_opt: Option<i64> = if let Some(chain_id) = chain_id_opt {
            ProcessChainRepo::resolve_step_id_by_process(&mut *conn, chain_id, req.next_process_id)
                .await?
        } else {
            None
        };
        let batch_rows = PartRepo::mark_batch_returned(
            &mut *conn,
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
        let part = PartRepo::get_by_id(&mut *conn, batch.part_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        // 4. PR-B2：part 派生列由 sync_from_batch_change 统一回填
        //    （location=PRODUCTION_SHELF / holder=shelf / next_process）。
        PartService::sync_from_batch_change(&mut *conn, part.id, current).await?;
        // 5. event
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id: part.id,
                event_type: "ADMIN_REMOVED_FROM_WORKER",
                from_status: Some("IN_PROCESS"),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: Some(&worker.badge_code),
                note: Some("admin_remove"),
                created_by: Some(current.id),
            },
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
        use crate::auth::rbac::Role;
        use crate::modules::process::repo::ProcessRepo;

        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        // 1. process 元数据
        let process = ProcessRepo::get_by_id(&mut *conn, process_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process {process_id} 不存在"),
                )
            })?;

        // 2. work_types
        let work_types =
            WorkTypeRepo::list_work_types_by_process_id(&mut *conn, process_id).await?;
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
        let worker_rows = WorkerRepo::list_active_by_process_id(&mut *conn, process_id).await?;
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
            WorkerPoolRepo::list_candidates_by_process_all_shelves(&mut *conn, process_id).await?;
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
    /// 按 `process_id + shelf_id` 范围，对每个匹配 worker 计算 target 并循环 refill：
    /// 1. 角色守卫：`Manager`（service 内 require_role）
    /// 2. 校验 `fill_ratio ∈ [0.0, 1.0]` → 20704
    /// 3. 校验 `process_id` 存在 → 20801
    /// 4. 取 process 映射的 work_types（`WorkTypeRepo::list_work_types_by_process_id`）
    ///    —— 同一 process 可能被多个 work_type 映射，每个 work_type 持有独立阈值
    /// 5. 取 `shelf_id` 货架上的 active worker 列表（复用现有 `WorkerRepo::list_active_by_process_id`）
    /// 6. 对每个 worker：
    ///    - 找到其所属 work_type 的 max 阈值（COUNT: `max_held_batches`；TIME: `max_held_minutes`）
    ///    - target = `(max × fill_ratio).ceil() as i32`
    ///    - 循环 `WorkerPoolRepo::take_one_from_pool` 直到 target 满 / 池空
    ///    - 每抢到一批写 `TAKEN_FROM_POOL` 事件 + `PartService::sync_from_batch_change`
    /// 7. 累计所有 worker 的 filled，组装 `AutoAllocateResult` 返回
    ///
    /// 错误码：
    /// - 20801 BIZ_PROCESS_NOT_FOUND
    /// - 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET（COUNT 模式但 work_type.max_held_batches IS NULL）
    /// - 20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET（TIME 模式但 work_type.max_held_minutes IS NULL）
    /// - 20704 BIZ_AUTO_ALLOCATE_INVALID_RATIO
    /// - 20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING（process 无 work_type）
    /// - 20201 BIZ_WORKER_NOT_FOUND / 20206 BIZ_WORKER_NO_WORK_TYPE（防御性）
    /// - 40300 FORBIDDEN
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
        let _process = ProcessRepo::get_by_id(&mut *conn, req.process_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process {} 不存在", req.process_id),
                )
            })?;

        // 3. process 映射的 work_types
        let work_types =
            WorkTypeRepo::list_work_types_by_process_id(&mut *conn, req.process_id).await?;
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
            // 二次查 max_held_minutes（list_work_types_by_process_id 未取该列）
            let row: Option<(Option<i32>,)> =
                sqlx::query_as("SELECT max_held_minutes FROM t_work_type WHERE id = $1")
                    .bind(wt_id)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(AppError::from)?;
            wt_max.insert(wt_id, (max_held_batches, row.and_then(|(v,)| v)));
        }

        // 4. 货架上的 active worker 列表（含 work_type_id）
        let worker_rows = WorkerRepo::list_active_by_process_id(&mut *conn, req.process_id).await?;

        let mut filled = Vec::with_capacity(worker_rows.len());
        let mut pool_empty_any = false;

        for (worker_id, _worker_name, work_type_id, _wt_code) in worker_rows {
            // list_active_by_process_id 已用 JOIN 过滤 `worker.work_type_id IS NOT NULL`
            // （详见 WorkerRepo::list_active_by_process_id 的 SQL），故 work_type_id
            // 必非 0 / 必非 NULL；防御性保留，但语义上一定有值。
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
                None => {
                    // 该 worker 所属 work_type 未映射本 process（理论上 list_active 已过滤）
                    continue;
                }
            };

            // 取该 worker 当前 badge_code（事件日志需要）
            let worker = match WorkerRepo::get_by_id(&mut *conn, worker_id, false).await? {
                Some(w) => w,
                None => continue,
            };
            if !worker.is_active {
                continue;
            }

            // 计算 target
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

            // 取该 work_type 映射的所有 process_ids（take_one_from_pool 限定）
            let process_ids = WorkTypeRepo::list_process_ids(&mut *conn, work_type_id).await?;
            if process_ids.is_empty() {
                continue;
            }

            // 循环 take_one_from_pool 直到 target / 池空
            let mut filled_count = 0i32;
            for _ in 0..target {
                match WorkerPoolRepo::take_one_from_pool(
                    &mut *conn,
                    worker_id,
                    req.shelf_id,
                    &process_ids,
                    current.id,
                )
                .await?
                {
                    Some(t) => {
                        PartRepo::insert_part_event(
                            &mut *conn,
                            NewPartEvent {
                                id: snowflake.next_id(),
                                part_id: t.part_id,
                                event_type: "TAKEN_FROM_POOL",
                                from_status: Some("IN_PROCESS"),
                                to_status: Some("IN_PROCESS"),
                                batch_id: Some(t.batch_id),
                                quantity: Some(t.quantity),
                                drawing_code: Some(&t.drawing_no),
                                badge_code: Some(&worker.badge_code),
                                note: Some("auto_allocate"),
                                created_by: Some(current.id),
                            },
                        )
                        .await?;
                        PartService::sync_from_batch_change(&mut *conn, t.part_id, current).await?;
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
    ///
    /// 2026-09-14 follow-up-ux 新增：补齐 UI 单 batch 拖拽缺口。
    /// 与 `refill_for_worker` 的差异：assign 是**单 batch 拖拽**语义——
    /// 不循环触顶 max_held_batches；只在 (worker_id, batch_id, shelf_id)
    /// 三元组命中候选池时切换 holder。
    ///
    /// 流程：
    /// 1. 角色守卫：Manager（service 内 require_role）
    /// 2. 校验 worker is_active + work_type_id 非空 + work_type.max_held_batches 已设
    ///    （与 refill 一致：未分配工种 / 未设上限都是错误）
    /// 3. 取 worker 当前持有批次数，若 ≥ max_held_batches →
    ///    `20204 BIZ_WORKER_HOLD_LIMIT_EXCEEDED`（assign 路径仍守 max 上限，
    ///    不允许单条拖拽触顶；前端 UI 显示当前 worker 的 capacity_remaining 提示）
    /// 4. 若 req.process_id.is_some()，校验 batch.next_process_id 必须匹配
    ///    （防止工人对未排到该工序的批做 assign）
    /// 5. `WorkerPoolRepo::take_specific_from_pool` → 若 None →
    ///    `20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER`（语义复用：batch 不在
    ///    候选池 = "不是 worker 可领取的批次"）
    /// 6. `PartService::sync_from_batch_change(part_id)` 同步 part 派生列
    /// 7. 写 `TAKEN_FROM_POOL` event（note="admin_assign"）
    /// 8. 返回 `AssignResult { worker_id, batch_id, shelf_id, taken, current_held, max_held }`
    pub async fn assign_batch_to_worker(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: AdminAssignRequest,
        current: &CurrentUser,
    ) -> Result<AssignResult, AppError> {
        current.require_role(Role::Manager)?;

        // 1. 取 worker（带 work_type + badge_code：worker_scan.rs 用法）
        let worker = WorkerRepo::get_by_id(&mut *conn, req.worker_id, false)
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
        let work_type = WorkTypeRepo::get_by_id(&mut *conn, work_type_id)
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

        // 2. capacity 守卫：assign 路径仍守 max_held_batches 上限（不循环触顶）。
        let current_held_before =
            PartBatchRepo::count_held_by_worker(&mut *conn, req.worker_id).await?;
        if current_held_before >= max_held as i64 {
            return Err(AppError::biz(
                code::BIZ_WORKER_HOLD_LIMIT_EXCEEDED,
                format!(
                    "worker {} 已持有 {} 批次，工种上限 {} 触顶",
                    req.worker_id, current_held_before, max_held
                ),
            ));
        }

        // 3. 可选 process_id 校验：若 req.process_id 提供，校验 batch 当前
        //    step.process_id 必须匹配（PR-3 批次 step 化：原 batch.next_process_id
        //    列已删，改为读 step.process_id；防止工人对未排到该工序的批做 assign）。
        if let Some(pid) = req.process_id {
            let batch = PartBatchRepo::get_by_id(&mut *conn, req.batch_id, false)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_PART_BATCH_NOT_FOUND,
                        format!("batch {} 不存在", req.batch_id),
                    )
                })?;
            // 解析 step.process_id（一次单行 SELECT）
            let step_process_id: Option<i64> = if let Some(step_id) = batch.current_process_step_id
            {
                sqlx::query_scalar(
                    "SELECT process_id FROM t_process_chain_step \
                     WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(step_id)
                .fetch_optional(&mut *conn)
                .await?
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

        // 4. 原子切换 holder：单 SQL 限定 (shelf_id, batch_id)
        let taken = WorkerPoolRepo::take_specific_from_pool(
            &mut *conn,
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

        // 5. part 派生列同步（PR-B2）
        PartService::sync_from_batch_change(&mut *conn, taken.part_id, current).await?;

        // 6. 写 TAKEN_FROM_POOL 事件日志（note="admin_assign" 区分 refill 来源）
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id: taken.part_id,
                event_type: "TAKEN_FROM_POOL",
                from_status: Some("IN_PROCESS"),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(taken.batch_id),
                quantity: Some(taken.quantity),
                drawing_code: Some(&taken.drawing_no),
                badge_code: Some(&worker.badge_code),
                note: Some("admin_assign"),
                created_by: Some(current.id),
            },
        )
        .await?;

        // 7. 返回结果：current_held = 分配后持有数（含本批次）
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
