//! part 域 worker-scan 流业务逻辑。
//!
//! 对应 Python myERP/api/v1/parts.py 中 `POST /parts/worker-scan` 端点
//! （Task 8）：worker 把持有件通过扫码台 RETURNED（放回生产架） / INSPECTED
//! （直接送检）。
//!
//! 本文件只承载 `PartService::worker_scan_event`（impl 块拆文件，Rust 允许
//! 同一 `impl Foo { ... }` 块分布在多个同 crate 文件中，编译器合并）。
//!
//! 实施约定：方法签名接收 `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## worker_scan_event
//! - 两分支 `WorkerScanEvent::{RETURNED, INSPECTED}`，分别走 mark_*_returned /
//!   mark_*_inspected（OCC）+ 写 `RETURNED_TO_SHELF` / `SENT_TO_INSPECTION` 事件。
//! - 不负责 refill：handler 在 commit 之前紧接着调
//!   `WorkerPoolService::refill_for_worker`（同事务），scan 与 refill 共享一个
//!   原子事务（OM-6 决议）。
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

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepo;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::shelf::repo::ShelfRepo;
use crate::modules::worker::repo::WorkerRepo;
use crate::modules::worker_pool::dto::WorkerScanEvent;
use crate::shared::error::{code, AppError};

use super::super::dto::{WorkerScanCoreOut, WorkerScanRequest};
use super::PartService;

impl PartService {
    /// worker-scan 共享核心（被单件端点 `POST /parts/worker-scan` 调用，Task 8）。
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
    /// `WorkerPoolService::refill_for_worker`（同事务）。这样 scan 与 refill 共享
    /// 一个原子事务（OM-6 决议），避免扫描放回 → refill 抢批中间被并发抢走
    /// 同批的竞争窗口。
    ///
    /// `WorkerScanEvent` 是 unit enum（`Copy`），所以 `req` 按值传（caller 的
    /// DTO `req.clone()` 不再需要）。
    #[allow(clippy::too_many_lines)]
    pub async fn worker_scan_event(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: WorkerScanRequest,
        current: &CurrentUser,
    ) -> Result<WorkerScanCoreOut, AppError> {
        // 1. shelf 校验（worker-scan shelf 必须 PRODUCTION 区 active；存在性 + zone 守卫）
        let _shelf = ShelfRepo::get_by_id_zone(&mut *conn, req.shelf_id, "PRODUCTION")
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {} 不存在或非 PRODUCTION 区", req.shelf_id),
                )
            })?;
        // 2. 反查 worker
        let worker = WorkerRepo::get_by_badge_code(&mut *conn, &req.badge_code, false)
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
        let part = PartRepo::get_by_serial(&mut *conn, &req.serial_no, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("serial_no {} 不存在", req.serial_no),
                )
            })?;
        // 4. 定位 batch（worker 持有 IN_PROCESS+WORKER）
        let bid_hint = req.batch_id.as_deref().and_then(|s| s.parse().ok());
        let batch = match PartRepo::find_worker_held_batch_for_part(
            &mut *conn,
            part.id,
            worker.id,
            bid_hint,
        )
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
                    "multiple IN_PROCESS batches held by this worker; specify batch_id in request body (see docs/api/parts.md#worker-scan)"
                        .to_string(),
                ));
            }
            Err(e) => return Err(AppError::from(e)),
        };
        // 5. event_type 分支
        let event_type_str: &'static str;
        match req.event_type {
            WorkerScanEvent::RETURNED => {
                // RETURNED 必须传 next_process_id
                let next_pid: i64 = req
                    .next_process_id
                    .as_deref()
                    .ok_or_else(|| AppError::validation("RETURNED 必须传 next_process_id"))?
                    .parse()
                    .map_err(|_| AppError::validation("next_process_id 非法"))?;
                // shelf ↔ process 映射校验（JOIN t_shelf_process）
                let maps: bool = sqlx::query_scalar!(
                    r#"SELECT EXISTS(
                        SELECT 1 FROM t_shelf_process
                        WHERE shelf_id = $1 AND process_id = $2
                    ) AS "exists!""#,
                    req.shelf_id,
                    next_pid,
                )
                .fetch_one(&mut *conn)
                .await?;
                if !maps {
                    return Err(AppError::biz(
                        code::BIZ_SHELF_PROCESS_NOT_MAPPED,
                        format!("shelf {} 不映射到工序 {}", req.shelf_id, next_pid),
                    ));
                }
                // 切 holder worker → shelf（OCC）
                let n = PartRepo::mark_batch_returned(
                    &mut *conn,
                    batch.id,
                    batch.version,
                    req.shelf_id,
                    next_pid,
                    Some(current.id),
                )
                .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                let n = PartRepo::mark_part_returned(
                    &mut *conn,
                    part.id,
                    part.version,
                    req.shelf_id,
                    next_pid,
                    Some(current.id),
                )
                .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                PartRepo::insert_part_event(
                    &mut *conn,
                    NewPartEvent {
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
                    },
                )
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
                let target = ShelfRepo::get_active_by_id(&mut *conn, target_id)
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
                let from = PartStatus::from_str(&part.status).ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "非法 part 状态")
                })?;
                if !from.can_transition_to(PartStatus::INSPECTION) {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_TRANSITION,
                        format!(
                            "part {} 当前状态 {} 不允许送检",
                            part.id,
                            from.as_str()
                        ),
                    ));
                }
                // 切 holder worker → target_shelf + 状态 IN_PROCESS → INSPECTION（OCC）
                let n = PartRepo::mark_batch_inspected(
                    &mut *conn,
                    batch.id,
                    batch.version,
                    target_id,
                    Some(current.id),
                )
                .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                let n = PartRepo::mark_part_inspected(
                    &mut *conn,
                    part.id,
                    part.version,
                    target_id,
                    Some(current.id),
                )
                .await?;
                if n == 0 {
                    return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
                }
                PartRepo::insert_part_event(
                    &mut *conn,
                    NewPartEvent {
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
                    },
                )
                .await?;
                event_type_str = "WORKER_SCAN_INSPECTED";
            }
        }
        Ok(WorkerScanCoreOut {
            worker_id: worker.id,
            part_id: part.id,
            batch_id: batch.id,
            event_type: event_type_str.to_string(),
            work_type_id,
            badge_code: worker.badge_code.clone(),
        })
    }
}
