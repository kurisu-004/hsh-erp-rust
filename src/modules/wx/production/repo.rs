//! wx::production 子模块 repo 层 —— SQL 真源
//!
//! 2026-10-11 新增：自旧 `src/modules/wx/repo.rs` 的 `BatchCountsAgg` /
//! `BatchList` / `WorkerStats` 三个结构搬入本文件（旧文件随本次删除）。
//!
//! ## 结构
//! ZST `ProductionRepo` + 4 个静态方法，收 `PgExecutor` 泛型（`batch_counts`
//! 例外，见下）、返 `sqlx::Error`（由 service 层映射 `AppError`）。不抽 trait ——
//! 四个只读查询没有需要 mock 的分支（沿 `wx::part_list::repo` /
//! `prod::process_design::repo` 的取舍）。
//!
//! ## ★ WHERE 只写一份（2026-10-11 消灭旧实现的漂移面）
//! 旧 `repo.rs` 把同一段 WHERE 手抄两遍（`list` 一份、`count` 一份）。本文件把
//! SELECT / FROM / WHERE / ORDER BY 拆成 4 个私有常量，**唯一**的差异
//! （`LIMIT` / `OFFSET`）在调用处拼。
//!
//! ⚠️ 更进一步：`count` **整个方法被删了** —— `hasMore` 改用「取 `size + 1` 条
//! 看是否超出」判定，前端从不读 `total`（`wx::part_list` 已是这个口径，见
//! `docs/api/wx.md` §3.4）。省掉一次 COUNT，也让两条 SQL 口径不可能再分叉。
//!
//! ⚠️ 但**角标 counts 仍是两条独立 SQL**（`batch_counts_by_period`），不共用
//! [`WHERE_CLAUSE`] —— 它是标量聚合、不能套列表的 `FROM` + `ORDER BY` + 分页。
//! 两条 count 与 list 的 **period 闸门逐字一致**，2026-10-11 review 第 1 轮又给
//! 它们补上了 `p.deleted_at IS NULL` 闸门（见 [`ProductionRepo::batch_counts_by_period`]
//! 的说明），使 counts 与 list 的**全部可见性口径**都对齐。
//!
//! ## 本域读到的 6 张表（跨域只读聚合，见 `docs/api/wx.md` §7）
//! `t_user` / `t_worker` / `t_work_type` / `t_part` / `t_part_batch` /
//! `t_part_event`。`t_user` 的 **SQL 真源属 iam 域**，但本域按本仓既定 pattern
//! （`statistics` / `admin` / `dashboard`）**只读聚合**，不 import iam 的
//! service / repo —— 护栏 `production_domain_depends_on_no_other_domain` 钉死。
//!
//! ## ⚠️ 无物理外键（仓库铁律）
//! `t_user.worker_id` 只是普通 `bigint` 列，指向的工人可能不存在 / 已软删。本文件
//! 用 **JOIN 谓词**（`w.id = u.worker_id AND w.deleted_at IS NULL`）而不是外键来
//! 把「指向了软删工人」收敛成「未绑定」（`worker: null` + `stats` 全 0，见
//! `super::service` 的「未绑定」段）。

use sqlx::{PgConnection, PgExecutor};

use super::model::{BatchCountsRow, ProductionBatchRow, WorkerRow, WorkerStatsRow};

/// `wx::production` ZST 静态方法容器。
pub struct ProductionRepo;

// ---- 列表 SQL 片段（4 个私有常量，`list_batches` 唯一拼装点）----------------

/// SELECT 列表（与 [`ProductionBatchRow`] 字段一一对应，alias 名逐字对齐字段名）。
///
/// ⚠️ `work_hours` 子查询**刻意不带** `COALESCE`：`SUM` 在无匹配行时返回 NULL，
/// 让「本月零工时」投影成 `null` 而不是 `0`（前端 `!= null` 守门依赖这个）。
const SELECT_COLS: &str = "SELECT
    b.id                            AS id,
    p.serial_no                     AS serial_no,
    p.name                          AS name,
    p.drawing_no                    AS drawing_no,
    b.batch_no                      AS batch_no,
    b.quantity                      AS quantity,
    b.status                        AS status,
    w.name                          AS assigned_to,
    p.planned_delivery_date         AS planned_delivery_date,
    (
        SELECT e_finished.created_at::date
        FROM t_part_event e_finished
        WHERE e_finished.batch_id = b.id
          AND e_finished.event_type = 'DELIVERED'
        ORDER BY e_finished.created_at DESC
        LIMIT 1
    )                               AS finished_date,
    (
        SELECT SUM(e_qty.quantity)::float8
        FROM t_part_event e_qty
        WHERE e_qty.batch_id = b.id
          AND e_qty.event_type IN ('PICKED_UP', 'RETURNED')
    )                               AS work_hours";

/// FROM 子句（批次 + 工单 + 当前持有人）。
///
/// ⚠️ `LEFT JOIN t_worker w ON w.id = b.current_holder_id AND b.location = 'WORKER'`
/// —— `location` 闸门**必须在 ON 里**（不能挪进 WHERE），否则批次挂在货架上时
/// 整行被过滤掉。批次在货架上 ⇒ `assigned_to` 为 `null`（前端 `assignedTo` 可空）。
const FROM_SQL: &str = "
    FROM t_part_batch b
    JOIN t_part p ON p.id = b.part_id
    LEFT JOIN t_worker w ON w.id = b.current_holder_id AND b.location = 'WORKER'";

/// WHERE 骨架（软删闸门 + tab 状态闸门 + period 闸门）。
///
/// ⚠️ **period 口径逐字沿用旧实现**（`BatchCountsAgg::by_period` 同款，两处必须
/// 保持一致，否则又会出现「角标与列表对不上」的老 bug）：
/// - `in_progress` 桶 → `b.status = 'IN_PROCESS' AND b.updated_at::text LIKE $2`
///   （批次最近一次 update 进入 IN_PROCESS 落在当月）
/// - `done` 桶 → `b.status IN ('DELIVERED','COMPLETED')` 且存在当月 `DELIVERED`
///   事件
///
/// ⚠️ 这里的 `p.deleted_at IS NULL`（配合 `FROM_SQL` 的 `JOIN t_part p`）**必须有**：
/// 缺了它，父工单软删、批次未软删的批次会「角标计入但列表不出现」。
/// `batch_counts_by_period` 的两条标量查询已在 2026-10-11 review 第 1 轮补上等价闸门。
///
/// `$1` 是 tab 映射出来的 DB 状态数组（`text[]`），`$2` 是 `YYYY-MM%` 的 LIKE
/// 模式串，**两个都是绑定变量** ⇒ 注入面为 0。
const WHERE_CLAUSE: &str = "
    WHERE b.status = ANY($1::text[])
      AND b.deleted_at IS NULL
      AND p.deleted_at IS NULL
      AND (
          (b.status = 'IN_PROCESS' AND b.updated_at::text LIKE $2)
          OR (
              b.status IN ('DELIVERED', 'COMPLETED')
              AND EXISTS (
                  SELECT 1 FROM t_part_event e
                  WHERE e.batch_id = b.id
                    AND e.event_type = 'DELIVERED'
                    AND e.created_at::text LIKE $2
              )
          )
      )";

/// ORDER BY 子句（最近变更优先；`id DESC` 兜底保证同秒变更的行之间顺序稳定）。
const ORDER_BY_CLAUSE: &str = " ORDER BY b.updated_at DESC, b.id DESC";

impl ProductionRepo {
    /// 登录账号 → 工人 → 工种（**★ 本次修掉的核心 bug 的数据源**）。
    ///
    /// 链路：`CurrentUser.id`（`t_user.id`）→ `t_user.worker_id` → `t_worker.id`
    /// → `t_work_type.name`。
    ///
    /// 返回 `None`（= 「未绑定」，service 层给出 `worker: null` + `stats` 全 0，
    /// **不报错**）的三种情形：
    /// 1. `t_user.worker_id IS NULL`（非工人账号 / 尚未绑定，占绝大多数）
    /// 2. `worker_id` 指向的工人行不存在（仓库无物理 FK，允许悬挂）
    /// 3. 工人行已软删（`w.deleted_at IS NOT NULL`）
    ///
    /// `t_work_type` 用 `LEFT JOIN … AND wt.deleted_at IS NULL`：工种被软删 / 未
    /// 分配工种都收敛成 `work_type_name = NULL`，service 层归一成空串（登记在
    /// `docs/api/wx.md` §8.7）。
    pub async fn find_worker_by_user<'e, E: PgExecutor<'e>>(
        executor: E,
        user_id: i64,
    ) -> Result<Option<WorkerRow>, sqlx::Error> {
        let row = sqlx::query_as!(
            WorkerRow,
            r#"
            SELECT u.worker_id     AS "worker_id!",
                   w.name          AS "name!",
                   wt.name         AS work_type_name
            FROM t_user u
            JOIN t_worker w
              ON w.id = u.worker_id
             AND w.deleted_at IS NULL
            LEFT JOIN t_work_type wt
              ON wt.id = w.work_type_id
             AND wt.deleted_at IS NULL
            WHERE u.id = $1
              AND u.deleted_at IS NULL
              AND u.worker_id IS NOT NULL
            "#,
            user_id,
        )
        .fetch_optional(executor)
        .await?;
        Ok(row)
    }

    /// 工人当月工作量（`t_part_event` 单条聚合查询）。
    ///
    /// - `batch_count` = 该工人当月发生过事件的**不同 `batch_id`** 数
    /// - `qty_sum` = 该工人当月 `PICKED_UP + RETURNED` 事件的 `SUM(quantity)`
    ///   （工作量估算；DB schema 无 `work_hours` 列）
    ///
    /// ⚠️ `worker_id` 参数是 **`t_worker.id`**，不是 `t_user.id` —— 旧实现传的是
    /// 后者，那正是本端点恒返 0 的原因（`find_worker_by_user` 解链后再传进来）。
    pub async fn worker_stats_by_period<'e, E: PgExecutor<'e>>(
        executor: E,
        worker_id: i64,
        period: &str, // YYYY-MM
    ) -> Result<WorkerStatsRow, sqlx::Error> {
        // LIKE 通配符：YYYY-MM → "YYYY-MM%"
        let pattern = format!("{period}%");
        let row = sqlx::query_as!(
            WorkerStatsRow,
            r#"
            SELECT
                COUNT(DISTINCT batch_id) FILTER (WHERE batch_id IS NOT NULL)
                    AS "batch_count!",
                COALESCE(
                    SUM(quantity) FILTER (WHERE event_type IN ('PICKED_UP', 'RETURNED')),
                    0
                )::bigint AS "qty_sum!"
            FROM t_part_event
            WHERE worker_id = $1
              AND created_at::text LIKE $2
            "#,
            worker_id,
            &pattern,
        )
        .fetch_one(executor)
        .await?;
        Ok(row)
    }

    /// 批次 tab 角标（当月 `in_progress` / `done` 两个计数）。
    ///
    /// ⚠️ 两条 SQL **逐字保留**自旧 `BatchCountsAgg::by_period`，只把返回结构换成
    /// [`BatchCountsRow`]（旧版直接返 VO，制造了 repo → vo 的依赖）。
    /// ⚠️ 唯一的改动是 2026-10-11 review 第 1 轮补的工单软删闸门（见下）。
    ///
    /// `finished_date` 派生口径：
    /// - `in_progress` = `status='IN_PROCESS'` AND `updated_at::text LIKE 'YYYY-MM%'`
    ///   （批次最近一次 update 进入 IN_PROCESS 状态在当月）
    /// - `done` = `status IN ('DELIVERED','COMPLETED')` AND 存在 batch 对应的
    ///   `t_part_event` 中 `event_type='DELIVERED'` AND 该事件
    ///   `created_at::text LIKE 'YYYY-MM%'`（批次在当月完成「实际送车」事件；
    ///   与 `dashboard` / `statistics` 域对齐）
    ///
    /// ⚠️ 这两条口径必须与 [`WHERE_CLAUSE`] 的 list 侧在 **period 闸门**上逐字一致，
    /// 否则角标与列表对不上（旧 `part_list` 域就栽在 198 vs 126 上，见
    /// `docs/api/wx.md` §3.3）。
    ///
    /// ⚠️⚠️ **2026-10-11 review 第 1 轮补的工单软删闸门**：这两条旧 SQL 只带
    /// `b.deleted_at IS NULL`，**不带** list 侧 `JOIN t_part p` 的 `p.deleted_at IS
    /// NULL`。分叉场景：**父工单已软删、批次未软删**时，`counts` 计入而 `list` 不出现
    /// （角标 > 列表实际行数）。该偏差从旧 `BatchCountsAgg::by_period` **逐字继承**
    /// （非本次重构引入的回归），但本次把口径收紧对齐 list 侧，并登记进
    /// `docs/api/wx.md` §8.10。闸门加在 **counts 侧**（`EXISTS(… p.deleted_at IS
    /// NULL)`）—— 因为这两条是**标量查询、不 JOIN `t_part`**，加 JOIN 会改变它们的
    /// 聚合形状；`EXISTS` 是与 list 侧 `INNER JOIN` 等价的半连接。
    ///
    /// 收 `&mut PgConnection`（不走 `E: PgExecutor`）以支持两次查询复用同一连接
    /// —— 与旧 `BatchCountsAgg::by_period`、`prod/batch/repo/queries.rs::_
    /// split_batch_inner` 同形。
    pub async fn batch_counts_by_period(
        conn: &mut PgConnection,
        period: &str, // YYYY-MM
    ) -> Result<BatchCountsRow, sqlx::Error> {
        // LIKE 通配符：YYYY-MM → "YYYY-MM%"
        let pattern = format!("{period}%");

        let in_progress: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch b
            WHERE b.status = 'IN_PROCESS'
              AND b.deleted_at IS NULL
              AND EXISTS (
                  SELECT 1 FROM t_part p
                  WHERE p.id = b.part_id
                    AND p.deleted_at IS NULL
              )
              AND b.updated_at::text LIKE $1
            "#,
            &pattern,
        )
        .fetch_one(&mut *conn)
        .await?;

        let done: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch b
            WHERE b.status IN ('DELIVERED', 'COMPLETED')
              AND b.deleted_at IS NULL
              AND EXISTS (
                  SELECT 1 FROM t_part p
                  WHERE p.id = b.part_id
                    AND p.deleted_at IS NULL
              )
              AND EXISTS (
                  SELECT 1 FROM t_part_event e
                  WHERE e.batch_id = b.id
                    AND e.event_type = 'DELIVERED'
                    AND e.created_at::text LIKE $1
              )
            "#,
            &pattern,
        )
        .fetch_one(&mut *conn)
        .await?;

        Ok(BatchCountsRow { in_progress, done })
    }

    /// 批次卡片列表（两个端点共用的**唯一**列表查询路径）。
    ///
    /// - `statuses`：service 层 tab 归一化后的 DB 状态集（非空；`?tab=` 是必填，
    ///   没有「不过滤」这一档）
    /// - `limit`：调用方传 `size + 1`（`hasMore` 靠「取超一条」判定，**不额外打
    ///   count 查询**）
    /// - `offset`：`(page - 1) * size`
    ///
    /// 用**非宏** `sqlx::query_as`（行结构手写 `FromRow`）：WHERE 需要与
    /// [`WHERE_CLAUSE`] 共用一份常量，而 `query!` 宏只接受字面量 SQL（不能用
    /// `const` 标识符），共用就得把 WHERE 手抄第二遍 —— 那正是本文件要消灭的
    /// 漂移。与 `wx::part_list::repo::list_parts` 的同形取舍一致。
    ///
    /// ⚠️ **注入面为 0**：`format!` 只填 4 个**编译期常量**（`SELECT_COLS` /
    /// `FROM_SQL` / `WHERE_CLAUSE` / `ORDER_BY_CLAUSE`），三个入参（`statuses` /
    /// `limit` / `offset`）一律走 bind，故 `AssertSqlSafe` 包裹安全 —— 与
    /// `wx::part_list::repo::list_parts` 的同一条理由。
    pub async fn list_batches<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        period: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ProductionBatchRow>, sqlx::Error> {
        let pattern = format!("{period}%");
        let sql =
            format!("{SELECT_COLS}{FROM_SQL}{WHERE_CLAUSE}{ORDER_BY_CLAUSE} LIMIT $3 OFFSET $4");
        sqlx::query_as::<_, ProductionBatchRow>(sqlx::AssertSqlSafe(sql))
            .bind(statuses)
            .bind(&pattern)
            .bind(limit)
            .bind(offset)
            .fetch_all(executor)
            .await
    }
}
