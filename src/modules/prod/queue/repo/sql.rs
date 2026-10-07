//! queue 域数据访问（SQL 真源，零 diff 搬迁自 `repo.rs`）
//!
//! 对应 Python myERP/repository/queue_repository.py。函数签名接收
//! `&mut PgConnection`（CTE 内含多条 SQL，发射多次借用同一连接）。
//!
//! - `take_one_from_pool` —— 工人「抢一批」原子 SQL。CTE + FOR UPDATE SKIP LOCKED，
//!   单条 SQL 内同时原子地完成：(1) 计算「已持批次数 < max_held_batches」守卫；
//!   (2) 从候选池按 system_delivery_date → planned_delivery_date → is_urgent → id
//!   优先级取一批；(3) UPDATE t_part_batch 与 t_part 的 holder/location/version。
//!   0 行 → 池空或达上限，返回 Ok(None)，由 service 决定是否抛容量/空池业务错。
//! - `take_specific_from_pool` —— 同上，但指定 batch_id（POOL→WORKER 移动用）
//! - `move_worker_to_worker` —— worker ↔ worker 移动（OCC）
//!
//! 2026-10-08：三个只读方法（`list_candidates_by_process_all_shelves` /
//! `list_held_by_worker_with_part` / `group_count_by_process_all_shelves`）已删，
//! 改由 [`crate::modules::prod::queue::board`] 的聚合 SQL 承担（那几个方法是
//! 逐工序 / 逐 worker 的，前端正是要靠它们发 N+1 个请求）。

use sqlx::PgConnection;

use crate::shared::error::AppError;

use crate::modules::prod::queue::vo::worker::TakenItem;

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
    /// 2026-09-29 新增：是否已上传 G_CODE 数控程序。
    /// 真相源：`EXISTS (SELECT 1 FROM t_part_file WHERE part_id = p.id AND kind = 'G_CODE' AND deleted_at IS NULL)`
    has_cnc_program: bool,
}

pub struct QueueRepo;

impl QueueRepo {
    /// 工人从其货架候选池「抢一批」（单 SQL 原子：held<max 守卫 + FOR UPDATE SKIP LOCKED）。
    ///
    /// 参数：
    /// - `conn`：调用方持有事务（handler 的 `state.pool.begin()`），repo 不 commit。
    /// - `worker_id`：目标工人 snowflake id（`t_worker.id`）。
    /// - `shelf_id`：工人所属货架（`t_shelf.id`），候选池按 `t_part_batch.current_holder_id = shelf_id` 过滤。
    /// - `process_ids`：工人可加工工序 id 列表（`t_process.id`），候选池按 `t_part_batch.current_process_id = ANY($3)` 过滤。
    /// - `operator_user_id`：审计字段 `updated_by`，由 service 透传（一般是 manager 自己）。
    ///
    /// 返回 `Ok(None)` 当且仅当：候选池为空 / 工人已达 `max_held_batches`。
    /// 不映射为 VersionConflict（其它并发抢同一批的事务会被 SKIP LOCKED 跳过，本事务拿到 0 行视为池空）。
    ///
    /// 2026-09-30 修复「下发后不入池」bug：候选 CTE 原本 INNER JOIN
    /// `t_process_chain_step s ON s.id = pb.current_process_step_id` 再按
    /// `s.process_id = ANY($3)` 过滤。dispatch 写 `step=NULL` → `s.id = NULL`
    /// 匹配不到任何行 → 批次对候选池隐身。现改为按
    /// `pb.current_process_id`（池归属权威依据）直接过滤，删掉 JOIN（顺带省
    /// 一次主键查找）。
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
                SELECT pb.id, pb.version, pb.part_id,
                       -- 2026-09-29 新增：has_cnc_program!（已上传 G_CODE → TRUE）。
                       -- 见 ORDER BY 第 1 键：已编程 batch 优先 take（编程员已完成
                       -- G_CODE 上传，下一步即可上机）。
                       EXISTS (SELECT 1 FROM t_part_file pf
                               WHERE pf.part_id = pb.part_id
                                 AND pf.kind = 'G_CODE'
                                 AND pf.deleted_at IS NULL) AS "has_cnc_program!"
                FROM t_part_batch pb
                JOIN t_part p ON p.id = pb.part_id
                -- 2026-09-30：候选池归属改按 pb.current_process_id 普通过滤
                --   （删 JOIN t_process_chain_step，见函数 doc 的 bug 修复说明）
                WHERE pb.status = 'IN_PROCESS'
                  AND pb.location = 'PRODUCTION_SHELF'
                  AND pb.current_holder_id = $2
                  AND pb.current_process_id = ANY($3)
                  AND pb.deleted_at IS NULL
                  AND p.deleted_at IS NULL
                  AND (SELECT n FROM held) < (SELECT max_held FROM max_batches)
                ORDER BY
                    -- 2026-09-29 新增：已编程 batch 优先（has_cnc_program DESC）。
                    -- 同交期同加急时，先把已上传 G_CODE 的工件派给工人，省
                    -- 「工人拿到手 → 还要等编程员传程序」这段等待。
                    EXISTS (SELECT 1 FROM t_part_file pf
                            WHERE pf.part_id = pb.part_id
                              AND pf.kind = 'G_CODE'
                              AND pf.deleted_at IS NULL) DESC,
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
                   sp.is_urgent, ub.version,
                   -- 2026-09-29 新增：从 candidate 透传 has_cnc_program
                   (SELECT "has_cnc_program!" FROM candidate WHERE candidate.id = ub.id) AS "has_cnc_program!"
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
            // 2026-09-29 新增：透传 has_cnc_program 到 TakenItem 出参（taken 部分）
            has_cnc_program: r.has_cnc_program,
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
    /// - `expected_version`：**客户端传来的 OCC 锚**（`MoveRequest.version`）。
    ///   2026-10-09 新增形参：此前本 SQL 的 OCC 是 `pb.version = candidate.version`
    ///   —— 拿 `FOR UPDATE` 锁住读到的行再拿它自己的 version 去比，等价于恒真，
    ///   并发改动会被这层自比悄悄吸收。改成灌客户端传值后，「看板 30s 快照已过期」
    ///   才真的被拒（0 行 → service 转 `40901 VERSION_CONFLICT`）。
    ///
    /// 返回：
    /// - `Ok(None)`：批次不在候选池（status ≠ IN_PROCESS / location ≠
    ///   PRODUCTION_SHELF / `current_holder_id` ≠ shelf_id / 已软删），
    ///   **或** `expected_version` 与库中现值不符
    /// - `Ok(Some(taken))`：抢到（含 part 元数据）
    /// - `Err(BIZ_WORKER_HOLD_LIMIT_EXCEEDED)`：由 service 守卫触发，本 repo 不抛
    pub async fn take_specific_from_pool(
        conn: &mut PgConnection,
        worker_id: i64,
        shelf_id: i64,
        batch_id: i64,
        expected_version: i32,
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, AppError> {
        let row: Option<TakenRow> = sqlx::query_as!(
            TakenRow,
            r#"
            WITH
            candidate AS (
                SELECT pb.id, pb.version, pb.part_id,
                       -- 2026-09-29 新增：与 take_one_from_pool 同源 EXISTS（admin_assign 不走优先级，但透传 has_cnc_program 给前端）
                       EXISTS (SELECT 1 FROM t_part_file pf
                               WHERE pf.part_id = pb.part_id
                                 AND pf.kind = 'G_CODE'
                                 AND pf.deleted_at IS NULL) AS "has_cnc_program!"
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
                    updated_at = NOW(), updated_by = $5
                FROM candidate
                WHERE pb.id = candidate.id
                  AND pb.version = candidate.version
                  -- 2026-10-09 新增：客户端传的 OCC 锚。candidate 里的
                  -- `pb.version = candidate.version` 是拿 FOR UPDATE 锁住的行比它自己，
                  -- 恒真，吸收并发改动；这一条才是真正的闸门。
                  AND pb.version = $4
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
                   sp.is_urgent, ub.version,
                   (SELECT "has_cnc_program!" FROM candidate WHERE candidate.id = ub.id) AS "has_cnc_program!"
            FROM upd_batch ub JOIN sel_part sp ON sp.id = ub.part_id
            "#,
            worker_id,
            shelf_id,
            batch_id,
            expected_version,
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
            // 2026-09-29 新增：透传 has_cnc_program
            has_cnc_program: r.has_cnc_program,
        }))
    }

    /// 列出某工序在所有生产货架上的候选批次（status=IN_PROCESS + location=PRODUCTION_SHELF
    /// + current_process_id=process_id）。
    ///
    /// 跨 4 表 JOIN：t_part_batch + t_part + t_customer L2 + t_customer L1 (LEFT JOIN)
    /// + t_shelf；service 在同事务内调用，repo 不 commit。
    ///
    /// 排序与 `take_one_from_pool` 对齐：admin 视图与工人抢一批用同一优先级，
    /// 业务上语义一致（先看交期，再看加急，最后看 id 稳定）。
    ///
    /// 不分页：admin 视角全量，前端按 shelf_id 客户端筛选。
    ///
    /// 2026-09-30 修复「下发后不入池」bug：`GET /prod/pool/{process_id}` 原本
    /// INNER JOIN `t_process_chain_step s2 ON s2.id = pb.current_process_step_id`
    /// 并按 `s2.process_id = $1` 过滤。dispatch 写 `step=NULL` → 批次隐身。
    /// 现改为 `pb.current_process_id = $1` 普通过滤，删掉 JOIN（顺带省一次
    /// 工人当前持有批次的「JOIN t_part + 客户/申请人/货架」完整 DTO（worker-pool state 端点用）。
    ///
    /// 2026-09-14 follow-up-ux 新增 → follow-up-round2 升级为 17 字段 `HeldBatchItem`：
    /// 解决上轮 `heldToCard` 字段降级（part_name/customer_name/applicant_name/
    /// location/shelf_code 等核心展示字段为空）。`WorkerPoolState` 一次性返回
    /// worker 当前持有的全部 batch，含全部展示字段，避免前端按 worker 轮询 K 次
    /// 单 batch 详情接口的 N+1。
    ///
    /// 2026-09-14 review 第 1 轮下沉：本函数原本位于 `part_batch/repo.rs`，
    /// 但它只被 queue 域消费（service 的 `compute_state` 调用一次），
    /// 把 queue 专用查询放到 part_batch 域违反了「owner 同域」原则，
    /// 同时导致 part_batch/repo.rs 行数越过 conventions.md 1000 行上限。
    /// 下沉到本域 repo，与 `take_one_from_pool` / `take_specific_from_pool` 同级。
    ///
    /// 2026-09-29 review 第 1 轮补漏：SELECT 增 `EXISTS (t_part_file kind='G_CODE')`
    /// 子查询透传 `has_cnc_program` → `HeldBatchItem.has_cnc_program`。
    /// 与 `take_one_from_pool` / `list_candidates_by_process_all_shelves` 同源 EXISTS，
    /// 保证 worker-pool state / 候选池视图 / 自动分配优先级三处口径一致。
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
    /// 走各自的 `id` / `name` 索引；EXISTS 子查询走 `ix_t_part_file_part_kind`
    /// 全工序候选批次聚合计数（2026-09-30 新增）。
    ///
    /// 单 SQL `GROUP BY current_process_id`：跨所有生产货架聚合 `t_part_batch` 中
    /// `status='IN_PROCESS' AND location='PRODUCTION_SHELF' AND deleted_at IS NULL`
    /// 的批次数（按工序维度统计）。
    ///
    /// 业务口径与 `list_candidates_by_process_all_shelves`（per-process 候选池详情）
    /// 完全一致：两者都限定 `status + location + deleted_at` 三态，唯一区别是本方法
    /// 只 GROUP BY 计次，不返回批次明细。
    ///
    /// 返回 `Vec<(i64, i64)>` 形态 `(process_id, count)`：service 层二次调
    /// `ProcessRepo::list_by_ids` 取 process_code / process_name 元数据后组装
    /// `QueueCountsOut`。repo 不做 process 元数据 JOIN 是有意为之——
    /// 与「本币 count GROUP BY」SQL 隔离，service 层负责 DTO 拼装，与
    /// `pool_by_process` service 路径同形态（先 GROUP BY 再二次查元数据）。
    ///
    /// 索引命中：`ix_t_part_batch_current_process_id`（current_process_id 部分
    /// 索引）+ `ix_t_part_batch_location`（`location`）+ `pb.deleted_at` 过滤。
    /// 本端点为 dashboard 快照型轻量查询（前端 WorkerQueueBoard tab 标题徽标），
    /// 无分页。
    ///
    /// 2026-09-30 修复「下发后不入池」bug：原本
    /// `JOIN t_process_chain_step s ON s.id = pb.current_process_step_id` +
    /// `GROUP BY s.process_id`，dispatch 写 `step=NULL` → 计数恒为 0。现改为
    /// worker ↔ worker 移动：把 batch 从源 worker 切到目标 worker（OCC，2026-09-30 新增）。
    ///
    /// `POST /api/v2/prod/pool/move` 当 from.kind=WORKER 且 to.kind=WORKER 时调此 SQL。
    /// - **不写 `current_process_step_id`**：move worker→worker 不推进工序链
    ///   （与 worker→pool 同原则；step 只在 worker-scan RETURNED/INSPECTED 推进）
    /// - **不写 `current_process_id`**：2026-09-30 写入不变式「池内移动工序不变」，
    ///   批次归还货架后仍属原工序候选池
    /// - WHERE 守卫：
    ///   - `current_holder_id = $4`：源 worker 必须当前持有该 batch
    ///   - `location = 'WORKER'`：与 holder 守卫配合限定状态机
    ///   - `status = 'IN_PROCESS'`：必须是加工中状态
    ///   - `version = $2`：乐观锁
    ///   - `deleted_at IS NULL`：排除软删
    /// - SET：current_holder_id = $3（dst）、location='WORKER'、version+1
    /// - 0 行 → `40901 VERSION_CONFLICT`（含 location/holder/status 不匹配）
    ///
    /// 注：handler 层 `tx.commit()` 之后会发 WS `WORKER_POOL_MOVE_DONE` 广播，
    /// 前端订阅统一事件名即可推断 from/to 方向（payload 含 src/dst worker id）。
    pub async fn move_worker_to_worker(
        conn: &mut PgConnection,
        batch_id: i64,
        src_worker_id: i64,
        dst_worker_id: i64,
        expected_version: i32,
        operator_user_id: Option<i64>,
    ) -> Result<u64, AppError> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET current_holder_id = $3,
                location          = 'WORKER',
                -- 2026-09-30 重构：worker↔worker move 不写 step
                --   （move 不推进工序链，与 worker→pool 同原则）
                -- 2026-09-30：同理不写 current_process_id（池内移动工序不变）
                version           = version + 1,
                updated_at        = NOW(),
                updated_by        = $5
            WHERE id = $1
              AND version = $2
              AND status = 'IN_PROCESS'
              AND location = 'WORKER'
              AND current_holder_id = $4
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            dst_worker_id,
            src_worker_id,
            operator_user_id as Option<i64>,
        )
        .execute(&mut *conn)
        .await
        .map_err(AppError::from)?;
        Ok(result.rows_affected())
    }
}
