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

use super::dto::PoolBatchItem;
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

/// `list_candidates_by_process_all_shelves` 行结构（JOIN 5 表后的扁平投影）。
///
/// 排序：system_delivery_date ASC NULLS LAST → planned_delivery_date → is_urgent DESC → id ASC。
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct CandidateRow {
    // —— t_part_batch ——
    batch_id: i64,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    location: String,
    placed_at: chrono::NaiveDateTime,
    batch_version: i32,
    // —— t_part ——
    serial_no: Option<String>,
    name: String,
    drawing_no: String,
    system_delivery_date: Option<chrono::NaiveDate>,
    is_urgent: bool,
    note: Option<String>,
    applicant_name: Option<String>,
    customer_id: Option<i64>,
    // —— t_customer L2 ——
    customer_name: Option<String>,
    parent_id: Option<i64>,
    // —— t_customer L1 (LEFT JOIN) ——
    parent_customer_name: Option<String>,
    // —— t_shelf ——
    shelf_id: i64,
    shelf_code: String,
    shelf_name: String,
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

    /// 列出某工序在所有生产货架上的候选批次（status=IN_PROCESS + location=PRODUCTION_SHELF
    /// + next_process_id=process_id）。
    ///
    /// 跨 5 表 JOIN：t_part_batch + t_part + t_customer L2 + t_customer L1 (LEFT JOIN)
    /// + t_shelf；service 在同事务内调用，repo 不 commit。
    ///
    /// 排序与 `take_one_from_pool` 对齐：admin 视图与工人抢一批用同一优先级，
    /// 业务上语义一致（先看交期，再看加急，最后看 id 稳定）。
    ///
    /// 不分页：admin 视角全量，前端按 shelf_id 客户端筛选。
    pub async fn list_candidates_by_process_all_shelves(
        conn: &mut PgConnection,
        process_id: i64,
    ) -> Result<Vec<PoolBatchItem>, AppError> {
        let rows: Vec<CandidateRow> = sqlx::query_as!(
            CandidateRow,
            r#"
            SELECT
                pb.id AS "batch_id!",
                pb.part_id AS "part_id!",
                pb.batch_no AS "batch_no!",
                pb.quantity AS "quantity!",
                pb.location AS "location!",
                pb.placed_at AS "placed_at!",
                pb.version AS "batch_version!",
                p.serial_no AS "serial_no?",
                p.name AS "name!",
                p.drawing_no AS "drawing_no!",
                p.system_delivery_date AS "system_delivery_date?",
                p.is_urgent AS "is_urgent!",
                p.note AS "note?",
                p.applicant_name AS "applicant_name?",
                p.customer_id AS "customer_id?",
                c.name AS "customer_name?",
                c.parent_id AS "parent_id?",
                cp.name AS "parent_customer_name?",
                s.id AS "shelf_id!",
                s.code AS "shelf_code!",
                s.name AS "shelf_name!"
            FROM t_part_batch pb
            JOIN t_part p ON p.id = pb.part_id
            LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL
            LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL
            JOIN t_shelf s ON s.id = pb.current_holder_id AND s.deleted_at IS NULL
            WHERE pb.status = 'IN_PROCESS'
              AND pb.location = 'PRODUCTION_SHELF'
              AND pb.next_process_id = $1
              AND pb.deleted_at IS NULL
              AND p.deleted_at IS NULL
            ORDER BY
                p.system_delivery_date ASC NULLS LAST,
                p.planned_delivery_date ASC NULLS LAST,
                p.is_urgent DESC,
                pb.id ASC
            "#,
            process_id,
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::from)?;

        Ok(rows
            .into_iter()
            .map(|r| {
                // 客户路径拼接：L1 自指仅给 leaf；有 L1 给 "L1 / L2"
                let customer_path = match (&r.parent_customer_name, &r.customer_name) {
                    (Some(p), Some(l)) => Some(format!("{p} / {l}")),
                    (_, Some(l)) => Some(l.clone()),
                    _ => None,
                };
                PoolBatchItem {
                    batch_id: r.batch_id,
                    part_id: r.part_id,
                    batch_no: r.batch_no,
                    quantity: r.quantity,
                    serial_no: r.serial_no,
                    name: r.name,
                    drawing_no: r.drawing_no,
                    system_delivery_date: r.system_delivery_date,
                    customer_name: r.customer_name,
                    parent_customer_name: r.parent_customer_name,
                    customer_path,
                    applicant_name: r.applicant_name,
                    location: r.location,
                    shelf_id: r.shelf_id,
                    shelf_code: r.shelf_code,
                    shelf_name: r.shelf_name,
                    is_urgent: r.is_urgent,
                    note: r.note,
                    placed_at: r.placed_at,
                    version: r.batch_version,
                }
            })
            .collect())
    }
}
