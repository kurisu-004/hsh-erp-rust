//! queue 域数据访问（SQL 真源，零 diff 搬迁自 `repo.rs`）
//!
//! 对应 Python myERP/repository/queue_repository.py。函数签名接收
//! `&mut PgConnection`（CTE 内含多条 SQL，发射多次借用同一连接）。
//!
//! - `take_one_from_pool` —— 工人「抢一批」原子 SQL。CTE + FOR UPDATE SKIP LOCKED，
//!   单条 SQL 内同时原子地完成：(1) 计算「已持批次数 < cap」守卫（cap 由 service
//!   传入，2026-10-15 起 = 工种自动补料上限兜底 `max_held_batches`）；
//!   (2) 从候选池按 `shared::shelf::pool_priority::POOL_PRIORITY_ORDER_SQL`
//!   （加急 → 系统交期 → 计划交期 → CNC 已编程）取一批；(3) UPDATE t_part_batch
//!   与 t_part 的 holder/location/version。
//!   0 行 → 池空或达 cap，返回 Ok(None)，由 service 决定是否抛容量/空池业务错。
//!   2026-10-10：`shelf_id` 形参改 `Option<i64>`，`None` = 跨全部货架取料。
//! - `take_specific_from_pool` —— 同上，但指定 batch_id（POOL→WORKER 移动用）
//! - `move_worker_to_worker` —— worker ↔ worker 移动（OCC）
//!
//! 2026-10-08：三个只读方法（`list_candidates_by_process_all_shelves` /
//! `list_held_by_worker_with_part` / `group_count_by_process_all_shelves`）已删，
//! 改由 [`crate::modules::prod::queue::board`] 的聚合 SQL 承担（那几个方法是
//! 逐工序 / 逐 worker 的，前端正是要靠它们发 N+1 个请求）。

use sqlx::{AssertSqlSafe, PgConnection};

use crate::shared::error::AppError;
use crate::shared::shelf::pool_priority::POOL_PRIORITY_ORDER_SQL;

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
    /// 工人「抢一批」（单 SQL 原子：held<cap 守卫 + FOR UPDATE SKIP LOCKED）。
    ///
    /// 参数：
    /// - `conn`：调用方持有事务（handler 的 `state.pool.begin()`），repo 不 commit。
    /// - `worker_id`：目标工人 snowflake id（`t_worker.id`）。
    /// - `shelf_id`：**2026-10-10 起是 `Option<i64>`**。`Some(sid)` 限架（管理员
    ///   显式「为某工人在某架上抢料」）；`None` **跨全部货架**取料。
    /// - `process_ids`：工人可加工工序 id 列表（`t_process.id`），候选池按
    ///   `t_part_batch.current_process_id = ANY($3)` 过滤。
    /// - `cap`：**2026-10-15 新增**：本次取料的目标持有上限（`held < cap` 守卫）。
    ///   由 service 计算：`auto_allocate_limit.unwrap_or(max_held_batches)`。
    ///   ⚠️ **NULL 语义与来源全在 service** —— 本 SQL 不再 JOIN `t_work_type`
    ///   查 `max_held_batches`：`max` 为 NULL 时 service 在入口就用
    ///   `20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET` 早退（refill / auto-allocate 的
    ///   auto=NULL 兜底也由 service 完成），**不存在「SQL 按 cap 0 拦死」这条
    ///   路径**（旧 `max_batches` CTE 的 `COALESCE(max_held_batches, 0)` 已随
    ///   本改动删除）。
    /// - `operator_user_id`：审计字段 `updated_by`，由 service 透传（一般是 manager 自己）。
    ///
    /// 返回 `Ok(None)` 当且仅当：候选池为空 / 工人已达 `cap`。
    /// 不映射为 VersionConflict（其它并发抢同一批的事务会被 SKIP LOCKED 跳过，本事务拿到 0 行视为池空）。
    ///
    /// 2026-09-30 修复「下发后不入池」bug：候选 CTE 原本 INNER JOIN
    /// `t_process_chain_step s ON s.id = pb.current_process_step_id` 再按
    /// `s.process_id = ANY($3)` 过滤。dispatch 写 `step=NULL` → `s.id = NULL`
    /// 匹配不到任何行 → 批次对候选池隐身。现改为按
    /// `pb.current_process_id`（池归属权威依据）直接过滤，删掉 JOIN（顺带省
    /// 一次主键查找）。
    ///
    /// ## 2026-10-10 三处变化
    ///
    /// 1. **不再有「架锚」**：候选 WHERE 的 `pb.current_holder_id = $2` 换成
    ///    `($2::bigint IS NULL OR pb.current_holder_id = $2)`。worker-scan 端点的
    ///    `shelf_id` 入参已删除（放回时由 `pick_least_loaded` 选架），refill 若还按架
    ///    过滤就会在「放回到 A 架、随即从 A 架补料」这个闭环里查空池。负载均衡的职责
    ///    已经整体移到 `pick_least_loaded`（放回时选架）一侧。
    /// 2. **ORDER BY 换成 `shared::shelf::pool_priority::POOL_PRIORITY_ORDER_SQL`**，
    ///    与看板池明细（`board/repo.rs::SQL_POOL_ITEMS_BY_PROCESS`）同一份。此前两处
    ///    各写一套且**已经漂移**（这里「已编程」排最前，那里「系统交期」排最前），
    ///    于是「看板上看到的顺序」与「工人实际抢到的顺序」对不上。
    /// 3. **新增 `LEFT JOIN t_process pr`**（供上面那条片段的第 4 层判 `is_cnc`）。
    ///
    /// ## 2026-10-14 一处变化：`held` CTE 补 `status = 'IN_PROCESS'`
    ///
    /// 判据的权威定义是 `docs/api/queue.md` §4.3「工人持有判据」，本 CTE 此前
    /// 只判 `current_holder_id` + `location = 'WORKER'` 两列，把
    /// `status = 'IN_PROCESS'` 漏掉了 —— 于是被取消 / 强制完成的批次（三个终态
    /// 状态下共 6 个写点此前都不清位置，逐点清单见 `docs/api/batch.md` §6.1）
    /// 照样吃工容量，**取消得越多工人越被卡死**，而队列板
    /// （`board/repo.rs::SQL_HELD_BATCHES_BY_WORKERS`，判据带 status）看不见它们。
    /// 现在本 CTE 与看板、`batch/repo/queries.rs::count_held_by_worker` 三处
    /// **逐字一致**。
    ///
    /// ## 2026-10-15 一处变化：cap 由 service 传入（原 `max_batches` CTE 删除）
    ///
    /// 守卫从 `AND (SELECT n FROM held) < (SELECT max_held FROM max_batches)`
    /// 改为 `AND (SELECT n FROM held) < $4`，`max_batches` CTE 与其对
    /// `t_work_type` 的 LEFT JOIN 一并删除。新语义：cap = 工种自动补料上限
    /// （`auto_allocate_limit`，NULL 时兜底 `max_held_batches`），由三条自动路径
    /// 的 service 统一算出；**工种未配 `max_held_batches` 的 20904 早退也在
    /// service**，SQL 层不再有「NULL 折 0 拦死」这条路径。手动单批路径
    /// （`take_specific_from_pool` / `pick-up`）仍按 `max_held_batches` 由
    /// `ensure_worker_has_capacity` 守卫，与 `cap` 无关。
    ///
    /// ⚠️ **守卫必须留在 SQL 内、不得上提到 Rust 层**：它承担的是**同一事务内
    /// 顺序取料的原子性** —— 单条语句内计算容量 + 选候选 + `FOR UPDATE SKIP
    /// LOCKED` 锁行 + UPDATE，循环里的第 N 次取料能看到前 N-1 次已 commit 的
    /// 自写行。⚠️ **但它不保证「跨事务并发下工人不会超发 1 批」**：两个并发事务
    /// 各自语句的 `held` 快照都早于对方 commit，`SKIP LOCKED` 只让两边锁不同
    /// 候选行 ⇒ 均可能通过守卫。真正串行化需要 worker 级锁（行锁 / advisory
    /// lock），与 `take_specific_from_pool` 的同类问题一起登记为独立重构，
    /// 见 `QueueService::ensure_worker_has_capacity` 的 doc。
    ///
    /// ## 为什么从 `query_as!` 改成运行时 `query` + `AssertSqlSafe`
    ///
    /// `POOL_PRIORITY_ORDER_SQL` 是一个**编译期常量**，但它要插进 CTE 的 `ORDER BY`
    /// 位置 —— `query_as!` 宏只吃 `&'static str` 字面量，没法在编译期做
    /// `concat!`。改成运行时 `query` + `format!` 填片段 + `AssertSqlSafe` 之后：
    /// 注入面为 0（用户输入一律走 bind，填进去的是一个编译期常量），代价是这条 SQL
    /// 不再进 `.sqlx/` 离线缓存（与 `board/repo.rs` 的选择同因）。
    pub async fn take_one_from_pool(
        conn: &mut PgConnection,
        worker_id: i64,
        shelf_id: Option<i64>,
        process_ids: &[i64],
        cap: i32,
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, AppError> {
        let sql = format!(
            r#"
            WITH
            held AS (
                -- 2026-10-14 补 status 过滤（修「容量误报」bug）：判据权威定义在
                -- `docs/api/queue.md` §4.3 = status='IN_PROCESS' AND location='WORKER'
                -- AND current_holder_id，**逐字**对齐看板
                -- `board/repo.rs::SQL_HELD_BATCHES_BY_WORKERS` 的 WHERE 与
                -- `batch/repo/queries.rs::count_held_by_worker`。此前本 CTE 只判
                -- 后两列，被取消 / 强制完成的批次（三个终态状态下共 6 个写点此前
                -- 都不清位置；逐点清单见 `docs/api/batch.md` §6.1）仍算数，
                -- 取消得越多工人越被卡死。
                SELECT COUNT(*)::int AS n FROM t_part_batch
                WHERE current_holder_id = $1
                  AND location = 'WORKER'
                  AND status = 'IN_PROCESS'
                  AND deleted_at IS NULL
            ),
            candidate AS (
                SELECT pb.id, pb.version, pb.part_id,
                       -- 已上传 G_CODE → TRUE。透传给 `TakenItem.has_cnc_program`
                       --（前端据此提示「可直接上机」）。
                       EXISTS (SELECT 1 FROM t_part_file pf
                               WHERE pf.part_id = pb.part_id
                                 AND pf.kind = 'G_CODE'
                                 AND pf.deleted_at IS NULL) AS has_cnc_program
                FROM t_part_batch pb
                JOIN t_part p ON p.id = pb.part_id
                LEFT JOIN t_process pr ON pr.id = pb.current_process_id
                -- 2026-09-30：候选池归属改按 pb.current_process_id 普通过滤
                --   （删 JOIN t_process_chain_step，见函数 doc 的 bug 修复说明）
                WHERE pb.status = 'IN_PROCESS'
                  AND pb.location = 'PRODUCTION_SHELF'
                  AND ($2::bigint IS NULL OR pb.current_holder_id = $2)
                  AND pb.current_process_id = ANY($3)
                  AND pb.deleted_at IS NULL
                  AND p.deleted_at IS NULL
                  -- 2026-10-15：cap 由 service 传入（auto_allocate_limit 兜底
                  -- max_held_batches），见函数 doc。
                  AND (SELECT n FROM held) < $4
                ORDER BY {POOL_PRIORITY_ORDER_SQL}
                LIMIT 1
                FOR UPDATE OF pb SKIP LOCKED
            ),
            upd_batch AS (
                UPDATE t_part_batch pb
                SET current_holder_id = $1, location = 'WORKER',
                    version = pb.version + 1,
                    updated_at = NOW(), updated_by = $5
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
                   -- 从 candidate 透传 has_cnc_program
                   (SELECT has_cnc_program FROM candidate WHERE candidate.id = ub.id) AS has_cnc_program
            FROM upd_batch ub JOIN sel_part sp ON sp.id = ub.part_id
            "#
        );
        let row: Option<TakenRow> = sqlx::query_as::<_, TakenRow>(AssertSqlSafe(sql))
            .bind(worker_id)
            .bind(shelf_id)
            .bind(process_ids)
            .bind(cap)
            .bind(operator_user_id)
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

    /// 2026-10-08 之前，本位置还挂着一大段 doc —— 描述
    /// `list_candidates_by_process_all_shelves` / `group_count_by_process_all_shelves`
    /// 两个**已删**只读方法（含「跨 4 表 JOIN：`t_customer c2` / `t_customer c1`
    /// LEFT JOIN 取 L1」等 JOIN 拓扑描述）。这两个方法已下线：① 2026-10-08 起职责
    /// 归 `prod::queue::board` 的聚合 SQL（见本文件模块 doc）；② 客户两列不再由
    /// SQL 自连取，改由 `shared::customer::paths_of` 批量补（口径见
    /// `docs/api/customers.md` §2.2）。保留这段只会让读代码的人去找一段不存在的
    /// JOIN，故整段删除。
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
