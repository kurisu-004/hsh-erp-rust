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
//!   候选池；调 `PartRepo::mark_batch_returned` + `mark_part_returned` + 写事件日志。

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepo;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::modules::work_type::repo::WorkTypeRepo;
use crate::modules::worker::repo::WorkerRepo;
use crate::shared::error::{code, AppError};

use super::dto::AdminRemoveRequest;
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

        let work_type = WorkTypeRepo::get_by_id(&mut *conn, work_type_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORK_TYPE_NOT_FOUND,
                    format!("work_type {work_type_id} 不存在"),
                )
            })?;
        let _max_held = work_type.max_held_batches.ok_or_else(|| {
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
                    badge_code: Some(&worker.badge_code),
                    note: None,
                    created_by: Some(operator_user_id),
                },
            )
            .await?;
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
            let n: i64 = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "n!" FROM t_part_batch pb
                WHERE pb.status = 'IN_PROCESS'
                  AND pb.location = 'PRODUCTION_SHELF'
                  AND pb.current_holder_id = $1
                  AND pb.next_process_id = $2
                  AND pb.deleted_at IS NULL"#,
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

        Ok(WorkerPoolState {
            worker_id,
            worker_name: worker.name,
            work_type_code: work_type.map(|w| w.code).unwrap_or_default(),
            max_held,
            current_held,
            capacity_remaining,
            pool_count_by_process,
        })
    }

    /// admin 主动把 worker 持有的某批次按 RETURNED 语义放回候选池。
    ///
    /// 流程：
    /// 1. 取 worker（事件日志 `badge_code` 需要）；
    /// 2. 按 `(batch_id, holder_id = worker_id)` 找 IN_PROCESS+WORKER 批次，
    ///    找不到 → `20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER`；
    /// 3. `mark_batch_returned` + `mark_part_returned`（OCC：version 冲突 →
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
                format!(
                    "batch {} 不是 worker {} 持有",
                    req.batch_id, req.worker_id
                ),
            )
        })?;
        // 3. 切 holder 到 shelf + 改 next_process_id（OCC）
        let batch_rows =
            PartRepo::mark_batch_returned(&mut *conn, batch.id, batch.version, req.shelf_id,
                req.next_process_id, Some(current.id)).await?;
        if batch_rows == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("batch {} 版本冲突或状态非 IN_PROCESS+WORKER", batch.id),
            ));
        }
        let part = PartRepo::get_by_id(&mut *conn, batch.part_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let part_rows = PartRepo::mark_part_returned(
            &mut *conn,
            part.id,
            part.version,
            req.shelf_id,
            req.next_process_id,
            Some(current.id),
        )
        .await?;
        if part_rows == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {} 版本冲突", part.id),
            ));
        }
        // 4. event
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
        // 5. 返回
        Ok(TakenItem {
            batch_id: batch.id,
            part_id: part.id,
            batch_no: batch.batch_no,
            quantity: batch.quantity,
            serial_no: part.serial_no,
            drawing_no: part.drawing_no,
            system_delivery_date: part.system_delivery_date,
            planned_delivery_date: part.planned_delivery_date,
            is_urgent: part.is_urgent,
            version: batch.version + 1,
        })
    }
}
