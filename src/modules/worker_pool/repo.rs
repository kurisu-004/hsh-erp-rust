//! worker_pool 域数据访问
//!
//! 对应 Python myERP/repository/worker_pool_repository.py。函数签名接收
//! `&mut PgConnection`（CTE 内含多条 SQL，发射多次借用同一连接）。
//!
//! Phase worker-pool-take：
//! - `take_one_from_pool` —— 工人「抢一批」原子 SQL。CTE + FOR UPDATE SKIP LOCKED，
//!   单条 SQL 内同时原子地完成：(1) 计算「已持批次数 < max_held_batches」守卫；
//!   (2) 从候选池按 system_delivery_date → planned_delivery_date → is_urgent → id
//!   优先级取一批；(3) UPDATE t_part_batch 与 t_part 的 holder/location/version。
//!   0 行 → 池空或达上限，返回 Ok(None)，由 service 决定是否抛容量/空池业务错。

use sqlx::PgConnection;

use crate::shared::error::AppError;

use super::model::TakenItem;

#[derive(Debug, sqlx::FromRow)]
struct TakenRow {
    batch_id: i64,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    serial_no: Option<String>,
    drawing_no: String,
    system_delivery_date: Option<chrono::NaiveDate>,
    planned_delivery_date: Option<chrono::NaiveDate>,
    is_urgent: bool,
    version: i32,
}

pub struct WorkerPoolRepo;

impl WorkerPoolRepo {
    /// 工人从其货架候选池「抢一批」（单 SQL 原子：held<max 守卫 + FOR UPDATE SKIP LOCKED）。
    ///
    /// 参数：
    /// - `conn`：调用方持有事务（handler 的 `state.pool.begin()`），repo 不 commit。
    /// - `worker_id`：目标工人 snowflake id（`t_worker.id`）。
    /// - `shelf_id`：工人所属货架（`t_shelf.id`），候选池按 `t_part_batch.current_holder_id = shelf_id` 过滤。
    /// - `process_ids`：工人可加工工序 id 列表（`t_process.id`），候选池按 `t_part_batch.next_process_id = ANY($3)` 过滤。
    /// - `operator_user_id`：审计字段 `updated_by`，由 service 透传（一般是 manager 自己）。
    ///
    /// 返回 `Ok(None)` 当且仅当：候选池为空 / 工人已达 `max_held_batches`。
    /// 不映射为 VersionConflict（其它并发抢同一批的事务会被 SKIP LOCKED 跳过，本事务拿到 0 行视为池空）。
    pub async fn take_one_from_pool(
        conn: &mut PgConnection,
        worker_id: i64,
        shelf_id: i64,
        process_ids: &[i64],
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, AppError> {
        let row: Option<TakenRow> = sqlx::query_as!(
            TakenRow,
            r#"
            WITH
            held AS (
                SELECT COUNT(*)::int AS n FROM t_part_batch
                WHERE current_holder_id = $1
                  AND location = 'WORKER' AND deleted_at IS NULL
            ),
            max_batches AS (
                SELECT COALESCE(wt.max_held_batches, 0) AS max_held
                FROM t_worker w LEFT JOIN t_work_type wt ON wt.id = w.work_type_id
                WHERE w.id = $1
            ),
            candidate AS (
                SELECT pb.id, pb.version, pb.part_id
                FROM t_part_batch pb
                JOIN t_part p ON p.id = pb.part_id
                WHERE pb.status = 'IN_PROCESS'
                  AND pb.location = 'PRODUCTION_SHELF'
                  AND pb.current_holder_id = $2
                  AND pb.next_process_id = ANY($3)
                  AND pb.deleted_at IS NULL
                  AND p.deleted_at IS NULL
                  AND (SELECT n FROM held) < (SELECT max_held FROM max_batches)
                ORDER BY
                    p.system_delivery_date ASC NULLS LAST,
                    p.planned_delivery_date ASC NULLS LAST,
                    p.is_urgent DESC,
                    pb.id ASC
                LIMIT 1
                FOR UPDATE OF pb SKIP LOCKED
            ),
            upd_batch AS (
                UPDATE t_part_batch pb
                SET current_holder_id = $1, location = 'WORKER',
                    version = pb.version + 1,
                    updated_at = NOW(), updated_by = $4
                FROM candidate
                WHERE pb.id = candidate.id
                  AND pb.version = candidate.version
                RETURNING pb.id AS batch_id, pb.part_id, pb.batch_no, pb.quantity, pb.version
            ),
            upd_part AS (
                UPDATE t_part p
                SET current_holder_id = $1, location = 'WORKER',
                    version = p.version + 1,
                    updated_at = NOW(), updated_by = $4
                FROM upd_batch ub
                WHERE p.id = ub.part_id
                RETURNING p.id, p.serial_no, p.drawing_no,
                          p.system_delivery_date, p.planned_delivery_date, p.is_urgent
            )
            SELECT ub.batch_id, ub.part_id, ub.batch_no, ub.quantity,
                   up.serial_no, up.drawing_no,
                   up.system_delivery_date, up.planned_delivery_date,
                   up.is_urgent, ub.version
            FROM upd_batch ub JOIN upd_part up ON up.id = ub.part_id
            "#,
            worker_id,
            shelf_id,
            process_ids,
            operator_user_id,
        )
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::from)?;
        Ok(row.map(|r| TakenItem {
            batch_id: r.batch_id,
            part_id: r.part_id,
            batch_no: r.batch_no,
            quantity: r.quantity,
            serial_no: r.serial_no,
            drawing_no: r.drawing_no,
            system_delivery_date: r.system_delivery_date,
            planned_delivery_date: r.planned_delivery_date,
            is_urgent: r.is_urgent,
            version: r.version,
        }))
    }
}
