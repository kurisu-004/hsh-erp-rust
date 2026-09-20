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

use sqlx::{PgConnection, PgExecutor};

use crate::shared::error::AppError;

use super::dto::PoolBatchItem;
use super::model::{HeldBatchItem, TakenItem};

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
///
/// 2026-09-16 PR-3 批次 step 化：删 `placed_at`（t_part_batch 列已删）。
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct CandidateRow {
    // —— t_part_batch ——
    batch_id: i64,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    location: String,
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
                -- 2026-09-16 PR-3 批次 step 化：next_process_id 列已删，
                -- 改为 JOIN t_process_chain_step s 取 process_id
                JOIN t_process_chain_step s ON s.id = pb.current_process_step_id
                WHERE pb.status = 'IN_PROCESS'
                  AND pb.location = 'PRODUCTION_SHELF'
                  AND pb.current_holder_id = $2
                  AND s.process_id = ANY($3)
                  AND pb.deleted_at IS NULL
                  AND p.deleted_at IS NULL
                  AND s.deleted_at IS NULL
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
                RETURNING pb.id, pb.part_id, pb.batch_no, pb.quantity, pb.version
            ),
            sel_part AS (
                SELECT p.id, p.serial_no, p.drawing_no,
                       p.system_delivery_date, p.planned_delivery_date, p.is_urgent
                FROM t_part p
                JOIN upd_batch ub ON ub.part_id = p.id
            )
            SELECT ub.id AS batch_id, ub.part_id, ub.batch_no, ub.quantity,
                   sp.serial_no, sp.drawing_no,
                   sp.system_delivery_date, sp.planned_delivery_date,
                   sp.is_urgent, ub.version
            FROM upd_batch ub JOIN sel_part sp ON sp.id = ub.part_id
            "#,
            worker_id,
            shelf_id,
            process_ids as &[i64],
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

    /// admin 端点「assign 单 batch」专用：从 `(shelf_id, batch_id)` 精确定位
    /// 候选池中的某一批，单 SQL 原子完成 holder 切换。
    ///
    /// 2026-09-14 follow-up-ux 新增：与 `take_one_from_pool` 的语义差异——
    /// 不按优先级排序候选，只取指定 `(batch_id, current_holder_id = shelf_id,
    /// status = IN_PROCESS, location = PRODUCTION_SHELF)` 的那一批；用于前端
    /// 单 batch 拖拽 UI（assign 端点）。
    ///
    /// **不**做 `held < max_held_batches` 守卫：该守卫下沉到 service
    /// (`assign_batch_to_worker`)，service 已在事务内先取 `current_held` 并
    /// 与 `max_held_batches` 比较，超额 → `20204 BIZ_WORKER_HOLD_LIMIT_EXCEEDED`。
    /// 这样 SQL 里的 held/max 守卫只剩 candidate CTE 里 `current_holder_id
    /// IS NOT NULL` 的隐式约束（candidate CTE 取不到 = batch 不在候选池）。
    ///
    /// 参数：
    /// - `worker_id`：目标 worker（写入 `t_part_batch.current_holder_id` + `t_part.derived`）
    /// - `shelf_id`：候选池货架（限定 `current_holder_id` 必须等于）
    /// - `batch_id`：唯一指定的批次
    /// - `operator_user_id`：审计字段 `updated_by`
    ///
    /// 返回：
    /// - `Ok(None)`：批次不在候选池（status ≠ IN_PROCESS / location ≠
    ///   PRODUCTION_SHELF / `current_holder_id` ≠ shelf_id / 已软删）
    /// - `Ok(Some(taken))`：抢到（含 part 元数据）
    /// - `Err(BIZ_WORKER_HOLD_LIMIT_EXCEEDED)`：由 service 守卫触发，本 repo 不抛
    pub async fn take_specific_from_pool(
        conn: &mut PgConnection,
        worker_id: i64,
        shelf_id: i64,
        batch_id: i64,
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, AppError> {
        let row: Option<TakenRow> = sqlx::query_as!(
            TakenRow,
            r#"
            WITH
            candidate AS (
                SELECT pb.id, pb.version, pb.part_id
                FROM t_part_batch pb
                JOIN t_part p ON p.id = pb.part_id
                WHERE pb.id = $3
                  AND pb.status = 'IN_PROCESS'
                  AND pb.location = 'PRODUCTION_SHELF'
                  AND pb.current_holder_id = $2
                  AND pb.deleted_at IS NULL
                  AND p.deleted_at IS NULL
                FOR UPDATE OF pb
            ),
            upd_batch AS (
                UPDATE t_part_batch pb
                SET current_holder_id = $1, location = 'WORKER',
                    version = pb.version + 1,
                    updated_at = NOW(), updated_by = $4
                FROM candidate
                WHERE pb.id = candidate.id
                  AND pb.version = candidate.version
                RETURNING pb.id, pb.part_id, pb.batch_no, pb.quantity, pb.version
            ),
            sel_part AS (
                SELECT p.id, p.serial_no, p.drawing_no,
                       p.system_delivery_date, p.planned_delivery_date, p.is_urgent
                FROM t_part p
                JOIN upd_batch ub ON ub.part_id = p.id
            )
            SELECT ub.id AS batch_id, ub.part_id, ub.batch_no, ub.quantity,
                   sp.serial_no, sp.drawing_no,
                   sp.system_delivery_date, sp.planned_delivery_date,
                   sp.is_urgent, ub.version
            FROM upd_batch ub JOIN sel_part sp ON sp.id = ub.part_id
            "#,
            worker_id,
            shelf_id,
            batch_id,
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
            -- 2026-09-16 PR-3 批次 step 化：JOIN step 取 process_id（替代列）
            JOIN t_process_chain_step s2 ON s2.id = pb.current_process_step_id
            WHERE pb.status = 'IN_PROCESS'
              AND pb.location = 'PRODUCTION_SHELF'
              AND s2.process_id = $1
              AND pb.deleted_at IS NULL
              AND p.deleted_at IS NULL
              AND s2.deleted_at IS NULL
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
                    // PR-3 批次 step 化：placed_at 列已删，不再展示
                    version: r.batch_version,
                }
            })
            .collect())
    }

    /// 工人当前持有批次的「JOIN t_part + 客户/申请人/货架」完整 DTO（worker-pool state 端点用）。
    ///
    /// 2026-09-14 follow-up-ux 新增 → follow-up-round2 升级为 17 字段 `HeldBatchItem`：
    /// 解决上轮 `heldToCard` 字段降级（part_name/customer_name/applicant_name/
    /// location/shelf_code 等核心展示字段为空）。`WorkerPoolState` 一次性返回
    /// worker 当前持有的全部 batch，含全部展示字段，避免前端按 worker 轮询 K 次
    /// 单 batch 详情接口的 N+1。
    ///
    /// 2026-09-14 review 第 1 轮下沉：本函数原本位于 `part_batch/repo.rs`，
    /// 但它只被 worker_pool 域消费（service 的 `compute_state` 调用一次），
    /// 把 worker_pool 专用查询放到 part_batch 域违反了「owner 同域」原则，
    /// 同时导致 part_batch/repo.rs 行数越过 conventions.md 1000 行上限。
    /// 下沉到本域 repo，与 `take_one_from_pool` / `take_specific_from_pool` 同级。
    ///
    /// JOIN 拓扑（与 `list_candidates_by_process_all_shelves` 的 5 表 JOIN 同形；
    /// t_applicant 用 `name = p.applicant_name` 因 t_part 无 applicant_id FK 字段）：
    /// - `t_part_batch pb`            主表
    /// - `t_part p`                   INNER JOIN（pb.part_id）
    /// - `t_customer c2`              LEFT JOIN（p.customer_id）—— L2 叶子客户
    /// - `t_customer c1`              LEFT JOIN（c2.parent_id）—— L1 一级集团
    /// - `t_applicant a`              LEFT JOIN（a.name = p.applicant_name）—— 申请人
    /// - `t_shelf s`                  LEFT JOIN（s.id = pb.current_holder_id）
    ///   WORKER 持有时 current_holder_id = worker_id 非 shelf_id，故 shelf_code
    ///   通常为 None
    ///
    /// 排序：按 `t_part_batch.id ASC`（与 `list_held_by_worker` 保持一致；前端按
    /// batch_id 稳定展示）。
    ///
    /// 索引命中：`ix_t_part_batch_holder_location` 覆盖 `(current_holder_id,
    /// location)` 谓词；JOIN t_part 走主键 `t_part.id`；JOIN t_customer / t_applicant
    /// 走各自的 `id` / `name` 索引。
    pub async fn list_held_by_worker_with_part<'e, E: PgExecutor<'e>>(
        executor: E,
        worker_id: i64,
    ) -> Result<Vec<HeldBatchItem>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"
            SELECT
                pb.id                  AS "batch_id!",
                pb.part_id             AS "part_id!",
                pb.batch_no            AS "batch_no!",
                pb.quantity            AS "quantity!",
                pb.location            AS "pb_location!",
                pb.version             AS "batch_version!",
                p.serial_no            AS "p_serial_no?",
                p.drawing_no           AS "p_drawing_no!",
                p.name                 AS "p_name!",
                p.system_delivery_date AS "p_system_delivery_date?",
                p.planned_delivery_date AS "p_planned_delivery_date!",
                p.is_urgent            AS "p_is_urgent!",
                p.note                 AS "p_note?",
                c2.name                AS "customer_name?",
                c1.name                AS "parent_customer_name?",
                a.name                 AS "applicant_name?",
                s.code                 AS "shelf_code?"
            FROM t_part_batch pb
            JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL
            LEFT JOIN t_customer c2 ON c2.id = p.customer_id AND c2.deleted_at IS NULL
            LEFT JOIN t_customer c1 ON c1.id = c2.parent_id AND c1.deleted_at IS NULL
            LEFT JOIN t_applicant a ON a.name = p.applicant_name AND a.deleted_at IS NULL
            LEFT JOIN t_shelf s ON s.id = pb.current_holder_id AND s.deleted_at IS NULL
            WHERE pb.status = 'IN_PROCESS'
              AND pb.location = 'WORKER'
              AND pb.current_holder_id = $1
              AND pb.deleted_at IS NULL
            ORDER BY pb.id ASC
            "#,
            worker_id,
        )
        .fetch_all(executor)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| HeldBatchItem {
                batch_id: r.batch_id,
                part_id: r.part_id,
                batch_no: r.batch_no,
                quantity: r.quantity,
                serial_no: r.p_serial_no,
                drawing_no: r.p_drawing_no,
                name: r.p_name,
                system_delivery_date: r.p_system_delivery_date,
                planned_delivery_date: Some(r.p_planned_delivery_date),
                is_urgent: r.p_is_urgent,
                customer_name: r.customer_name,
                parent_customer_name: r.parent_customer_name,
                applicant_name: r.applicant_name,
                location: r.pb_location,
                shelf_code: r.shelf_code,
                note: r.p_note,
                version: r.batch_version,
            })
            .collect())
    }
}
