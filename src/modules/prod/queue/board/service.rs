//! prod::queue 队列板装配 service（2026-10-08 新增）
//!
//! service 层零 SQL：只做角色守卫 + 分组 + 派生字段计算。SQL 全在
//! [`super::repo::QueueBoardRepo`]。
//!
//! ## 派生字段在 service 算、不在 SQL 算
//! - `capacity_remaining = max(0, max_held - current_held)`：`max_held` 来自
//!   `t_work_type`（worker JOIN 带出），`current_held` 来自持有批次行数。
//!   两者在 SQL 里不同源（一个是每 worker 一行、一个是 `ANY` 数组的全部行），
//!   在 SQL 里算就得让持有批次子查询按 worker_id GROUP BY 后再 LEFT JOIN，
//!   那是把「一次 ANY 取齐」退化成 GROUP BY + JOIN —— 收益为零、复杂度上升。
//! - `current_held` = 持有行按 `holder_id` 分组后的计数（repo 已在 SQL 里
//!   `ORDER BY pb.current_holder_id ASC` 保证同 worker 的行连续，但这里不依赖
//!   那个顺序，用 HashMap 显式分组）。

use std::collections::HashMap;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_shanghai_iso;
use crate::shared::error::AppError;

use super::repo::{HeldBatchRow, PoolItemRow, ProcessMetaRow, QueueBoardRepo, WorkerRow};
use crate::modules::prod::queue::vo::board::{
    QueueBoardSnapshot, QueueHeldBatch, QueuePoolItem, QueueProcessBoard, QueueProcessBoardDetail,
    QueueProcessMeta, QueueWorkerBrief,
};

/// 队列板装配 service（unit struct；service 不持 repo / pool —— handler 借
/// `&mut *conn` 传入）。
pub struct QueueBoardService;

impl Default for QueueBoardService {
    fn default() -> Self {
        Self
    }
}

impl QueueBoardService {
    pub fn new() -> Self {
        Self
    }

    /// 工序序列板。角色守卫：Manager + Clerk + Inspector（沿用被取代的
    /// `GET /pool/counts` 口径 —— admin 视角但不止 Manager）。
    pub async fn build_snapshot(
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<QueueBoardSnapshot, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let (counts, meta, pending) = QueueBoardRepo::board_snapshot(&mut *conn).await?;
        // process_id → 元数据。查不到的（工序已软删）走占位名，计数仍要显示。
        let mut meta_by_id: HashMap<i64, ProcessMetaRow> =
            meta.into_iter().map(|m| (m.id, m)).collect();
        let processes: Vec<QueueProcessBoard> = counts
            .into_iter()
            .map(|c| {
                let m = meta_by_id.remove(&c.process_id);
                let (code, name, color, category) = match m {
                    Some(m) => (m.code, m.name, m.color, m.category),
                    None => (
                        String::new(),
                        format!("(deleted#{})", c.process_id),
                        None,
                        String::new(),
                    ),
                };
                QueueProcessBoard {
                    process_id: c.process_id.to_string(),
                    process_code: code,
                    process_name: name,
                    color,
                    category,
                    pool_count: c.count,
                }
            })
            .collect();
        Ok(QueueBoardSnapshot {
            processes,
            pending_count: pending,
            ts: now_shanghai_iso(),
        })
    }

    /// 单工序队列板。角色守卫同 [`Self::build_snapshot`]。
    pub async fn build_process_detail(
        conn: &mut PgConnection,
        current: &CurrentUser,
        process_id: i64,
    ) -> Result<QueueProcessBoardDetail, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let data = QueueBoardRepo::board_process_detail(&mut *conn, process_id).await?;

        // 持有批次按 worker 分组（一次 ANY 查询结果的服务端分组，不是第二次查询）
        let mut held_by_worker: HashMap<i64, Vec<QueueHeldBatch>> = HashMap::new();
        for row in data.held {
            held_by_worker
                .entry(row.holder_id)
                .or_default()
                .push(to_held_batch(row));
        }

        let workers: Vec<QueueWorkerBrief> = data
            .workers
            .into_iter()
            .map(|w| {
                let held = held_by_worker.remove(&w.worker_id).unwrap_or_default();
                to_worker_brief(w, held)
            })
            .collect();

        let items: Vec<QueuePoolItem> = data.items.into_iter().map(to_pool_item).collect();
        let total = items.len() as i64;

        Ok(QueueProcessBoardDetail {
            process: QueueProcessMeta {
                process_id: data.process.id.to_string(),
                process_code: data.process.code,
                process_name: data.process.name,
                color: data.process.color,
            },
            workers,
            items,
            total,
            ts: now_shanghai_iso(),
        })
    }
}

fn to_held_batch(r: HeldBatchRow) -> QueueHeldBatch {
    QueueHeldBatch {
        batch_id: r.batch_id.to_string(),
        part_id: r.part_id.to_string(),
        batch_no: r.batch_no,
        quantity: r.quantity,
        serial_no: r.serial_no,
        name: r.name,
        drawing_no: r.drawing_no,
        system_delivery_date: r.system_delivery_date,
        planned_delivery_date: r.planned_delivery_date,
        is_urgent: r.is_urgent,
        has_cnc_program: r.has_cnc_program,
        customer_name: r.customer_name,
        parent_customer_name: r.parent_customer_name,
        applicant_name: r.applicant_name,
        location: r.location,
        note: r.note,
        version: r.version,
    }
}

fn to_worker_brief(w: WorkerRow, held_batches: Vec<QueueHeldBatch>) -> QueueWorkerBrief {
    // 工种未设 `max_held_batches` 时按 0 处理（与既有 `compute_state` 口径一致：
    // 不报错、只是 capacity 恒 0）。
    let max_held = w.max_held.unwrap_or(0);
    let current_held = held_batches.len() as i32;
    QueueWorkerBrief {
        worker_id: w.worker_id.to_string(),
        name: w.name,
        work_type_code: w.work_type_code,
        badge_code: w.badge_code,
        max_held,
        current_held,
        // 已被超量分配的历史数据（max_held 改小）会让 max-current 为负，
        // 展示负容量会让 UI 渲染出「-2 个空位」，故 clamp 到 0。
        capacity_remaining: (max_held - current_held).max(0),
        held_batches,
    }
}

fn to_pool_item(r: PoolItemRow) -> QueuePoolItem {
    QueuePoolItem {
        batch_id: r.batch_id.to_string(),
        part_id: r.part_id.to_string(),
        batch_no: r.batch_no,
        quantity: r.quantity,
        serial_no: r.serial_no,
        name: r.name,
        drawing_no: r.drawing_no,
        system_delivery_date: r.system_delivery_date,
        customer_name: r.customer_name,
        parent_customer_name: r.parent_customer_name,
        applicant_name: r.applicant_name,
        shelf_id: r.shelf_id.to_string(),
        shelf_code: r.shelf_code,
        shelf_name: r.shelf_name,
        is_urgent: r.is_urgent,
        has_cnc_program: r.has_cnc_program,
        note: r.note,
        version: r.version,
    }
}
