//! prod::queue 队列板聚合 SQL（2026-10-08 新增）
//!
//! ## 为什么不用 `query!` 宏
//!
//! 照 `dashboard` 域的做法：复杂聚合 SQL 字段多、迭代频繁，不进 `.sqlx/` 离线
//! 缓存（改一个字段要重跑 `sqlx_prepare.sh` 提交一批 hash 文件）。本文件全部走
//! 运行时 `sqlx::query` + `Row::get`。
//!
//! ## 三条铁律
//!
//! 1. **SQL 写成模块级 `const SQL_*` 字面量，不做字符串拼接。** 拼列名/拼条件会
//!    开注入面，也让 SQL 文本不再可被静态检查。两种口径（如 planned / system）
//!    就写两段完整字面量。
//!    **唯一例外**：`{has_process_chain}` 占位符填的是
//!    `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR` 这一个**编译期常量**（4 处
//!    卡片 DTO 共用同一判据，各自复制表达式文本等于让四处各漂一次），填完走
//!    `AssertSqlSafe`。注入面为 0：用户输入一律走 bind。
//! 2. **SQL 条数固定，与工人数 / 批次数无关。** 逐工人循环查是本文件要消灭的
//!    东西（`GET /queue/state` 时代前端要发 N+1 个请求）。每个方法的 doc 写明
//!    「固定 N 条」。
//! 3. **时间口径从 service 层绑进来，不写 SQL 的 `CURRENT_DATE`。** DB 会话时区
//!    与本仓统一的 Asia/Shanghai（`infra::clock::now_naive()`）是两个时钟；
//!    测试容器会话时区正是 UTC，两者不一致时会静默丢行。本文件 2 个方法都用
//!    不到日期，故只有 `count_pending_batches` 之外的查询没有时间窗口 ——
//!    一旦将来加窗口，必须走形参。
//!
//! ## 域隔离
//!
//! 本文件**不 import 任何其它域**的 service / repo：读的 7 张表
//! （`t_part_batch` / `t_process` / `t_worker` / `t_work_type` /
//! `t_work_type_process` / `t_part` / `t_customer` / `t_applicant` / `t_shelf`）
//! 全部在本域 SQL 内聚合。护栏见 `super::mod` 的
//! `board_aggregation_depends_on_no_other_domain` 单测。

use sqlx::{AssertSqlSafe, PgConnection, Row};

use crate::shared::batch::chain::HAS_PROCESS_CHAIN_EXPR;
use crate::shared::error::{AppError, code};

// ---------------------------------------------------------------------------
// SQL 常量
// ---------------------------------------------------------------------------

/// `board_snapshot` SQL 1：各工序候选池计数（跨所有生产货架）。
///
/// 候选池的**唯一**判据：`status='IN_PROCESS' AND location='PRODUCTION_SHELF'`。
/// 加上 `current_process_id IS NOT NULL`（丢弃「池归属为空」的批次 —— 它们不属
/// 任何工序，GROUP BY 会产出一个 NULL 组而解码进 `i64` 直接报错）。
const SQL_POOL_COUNT_BY_PROCESS: &str = "SELECT current_process_id AS process_id, \
     COUNT(*)::bigint AS cnt \
     FROM t_part_batch \
     WHERE status = 'IN_PROCESS' \
       AND location = 'PRODUCTION_SHELF' \
       AND deleted_at IS NULL \
       AND current_process_id IS NOT NULL \
     GROUP BY current_process_id \
     ORDER BY current_process_id ASC";

/// `board_snapshot` SQL 2：工序元数据（一次 `id = ANY($1)` 批量查，零 N+1）。
///
/// `color` 可空（迁移后新列，历史行为 NULL）；`category` 是 DB CHECK 约束枚举
/// `INHOUSE` / `OUTSOURCE`。
const SQL_PROCESS_META_BY_IDS: &str = "SELECT id, code, name, color, category \
     FROM t_process \
     WHERE id = ANY($1::bigint[]) \
       AND deleted_at IS NULL \
     ORDER BY id ASC";

/// `board_snapshot` SQL 3：待下发批次数。
///
/// 口径与 `GET /api/v2/prod/queue/pending` 的 `count_pending_batches` 逐字一致
/// （`IN ('PENDING','PROGRAMMING')` + 两侧软删闸门）—— 两个数字必须在同一页上
/// 对得上。
const SQL_COUNT_PENDING: &str = "SELECT COUNT(*)::bigint AS cnt \
     FROM t_part_batch pb \
     JOIN t_part p ON p.id = pb.part_id \
     WHERE pb.status IN ('PENDING', 'PROGRAMMING') \
       AND pb.deleted_at IS NULL \
       AND p.deleted_at IS NULL";

/// `board_process_detail` SQL 1：工序元数据（单行）。
const SQL_PROCESS_META_ONE: &str = "SELECT id, code, name, color \
     FROM t_process \
     WHERE id = $1 \
       AND deleted_at IS NULL";

/// `board_process_detail` SQL 2：该工序可用的工人（`t_worker` ×
/// `t_work_type_process` × `t_work_type`）。
///
/// 闸门：`w.is_active` + `w.deleted_at IS NULL`（工人）+ `wtp.deleted_at IS NULL`
/// （映射）+ `wt.deleted_at IS NULL`（工种）。`work_type_id IS NULL` 的工人
/// INNER JOIN 工种表时被自然排除 —— 没有工种就没有 max_held，无法参与容量计算。
const SQL_WORKERS_BY_PROCESS: &str = "SELECT w.id AS worker_id, \
     w.name AS worker_name, \
     w.badge_code AS badge_code, \
     wt.code AS work_type_code, \
     wt.max_held_batches AS max_held \
     FROM t_worker w \
     JOIN t_work_type_process wtp ON wtp.work_type_id = w.work_type_id \
                                  AND wtp.process_id = $1 \
                                  AND wtp.deleted_at IS NULL \
     JOIN t_work_type wt ON wt.id = w.work_type_id AND wt.deleted_at IS NULL \
     WHERE w.is_active = TRUE \
       AND w.deleted_at IS NULL \
     ORDER BY w.id ASC";

/// `board_process_detail` SQL 3：**一次**取齐该工序全部工人的持有批次。
///
/// ⚠️ 这是消灭 N+1 的关键：`current_holder_id = ANY($1::bigint[])`（不是
/// `= $1`），10 个工人与 2 个工人发的是**同一条 SQL**，只是数组长度不同。
/// 闸门与 `take_one_from_pool` 的持有态判定一致：`status='IN_PROCESS' AND
/// location='WORKER'`。
const SQL_HELD_BATCHES_BY_WORKERS: &str = "SELECT pb.id AS batch_id, \
     pb.part_id AS part_id, \
     pb.batch_no AS batch_no, \
     pb.quantity AS quantity, \
     pb.location AS location, \
     pb.version AS version, \
     pb.current_holder_id AS holder_id, \
     p.serial_no AS serial_no, \
     p.name AS name, \
     p.drawing_no AS drawing_no, \
     p.system_delivery_date AS system_delivery_date, \
     p.planned_delivery_date AS planned_delivery_date, \
     p.is_urgent AS is_urgent, \
     p.note AS note, \
     c2.name AS customer_name, \
     c1.name AS parent_customer_name, \
     a.name AS applicant_name, \
     EXISTS (SELECT 1 FROM t_part_file pf \
             WHERE pf.part_id = pb.part_id \
               AND pf.kind = 'G_CODE' \
               AND pf.deleted_at IS NULL) AS has_cnc_program, \
     {has_process_chain} AS has_process_chain \
     FROM t_part_batch pb \
     JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
     LEFT JOIN t_customer c2 ON c2.id = p.customer_id AND c2.deleted_at IS NULL \
     LEFT JOIN t_customer c1 ON c1.id = c2.parent_id AND c1.deleted_at IS NULL \
     LEFT JOIN t_applicant a ON a.name = p.applicant_name AND a.deleted_at IS NULL \
     LEFT JOIN t_process_chain_step cs \
       ON cs.id = pb.current_process_step_id AND cs.deleted_at IS NULL \
     WHERE pb.status = 'IN_PROCESS' \
       AND pb.location = 'WORKER' \
       AND pb.current_holder_id = ANY($1::bigint[]) \
       AND pb.deleted_at IS NULL \
     ORDER BY pb.current_holder_id ASC, pb.id ASC";

/// `board_process_detail` SQL 4：该工序候选池（跨所有生产货架）。
///
/// `has_process_chain` 由 `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR` 提供，
/// 判据与理由（含「必须 `IS NOT NULL AND =` 而不是 `IS NOT DISTINCT FROM`」）见
/// 该常量。`t_process_chain_step cs` 走 **LEFT JOIN** —— 无 step 的批次（PENDING /
/// 指针为空）必须照样出现在列表里。
const SQL_POOL_ITEMS_BY_PROCESS: &str = "SELECT pb.id AS batch_id, \
     pb.part_id AS part_id, \
     pb.batch_no AS batch_no, \
     pb.quantity AS quantity, \
     pb.version AS version, \
     p.serial_no AS serial_no, \
     p.name AS name, \
     p.drawing_no AS drawing_no, \
     p.system_delivery_date AS system_delivery_date, \
     p.is_urgent AS is_urgent, \
     p.note AS note, \
     p.applicant_name AS applicant_name, \
     c2.name AS customer_name, \
     c1.name AS parent_customer_name, \
     a.name AS applicant_name_resolved, \
     s.id AS shelf_id, \
     s.code AS shelf_code, \
     s.name AS shelf_name, \
     EXISTS (SELECT 1 FROM t_part_file pf \
             WHERE pf.part_id = pb.part_id \
               AND pf.kind = 'G_CODE' \
               AND pf.deleted_at IS NULL) AS has_cnc_program, \
     {has_process_chain} AS has_process_chain \
     FROM t_part_batch pb \
     JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
     LEFT JOIN t_customer c2 ON c2.id = p.customer_id AND c2.deleted_at IS NULL \
     LEFT JOIN t_customer c1 ON c1.id = c2.parent_id AND c1.deleted_at IS NULL \
     LEFT JOIN t_applicant a ON a.name = p.applicant_name AND a.deleted_at IS NULL \
     JOIN t_shelf s ON s.id = pb.current_holder_id AND s.deleted_at IS NULL \
     LEFT JOIN t_process_chain_step cs \
       ON cs.id = pb.current_process_step_id AND cs.deleted_at IS NULL \
     WHERE pb.status = 'IN_PROCESS' \
       AND pb.location = 'PRODUCTION_SHELF' \
       AND pb.current_process_id = $1 \
       AND pb.deleted_at IS NULL \
     ORDER BY p.system_delivery_date ASC NULLS LAST, \
              p.is_urgent DESC, \
              pb.id ASC";

// ---------------------------------------------------------------------------
// 行精简（repo ↔ service 边界）
// ---------------------------------------------------------------------------

/// 工序计数行。
pub struct ProcessCountRow {
    pub process_id: i64,
    pub count: i64,
}

/// 工序元数据行。
pub struct ProcessMetaRow {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub color: Option<String>,
    /// 仅 `board_snapshot` 查（单工序详情不需要 category 展示）
    pub category: String,
}

/// 工人行（`board_process_detail` SQL 2）。
pub struct WorkerRow {
    pub worker_id: i64,
    pub name: String,
    pub badge_code: String,
    pub work_type_code: String,
    /// `t_work_type.max_held_batches`，可空（未设置时 service 层按 0 处理）
    pub max_held: Option<i32>,
}

/// 持有批次行（`board_process_detail` SQL 3）。
pub struct HeldBatchRow {
    pub holder_id: i64,
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub location: String,
    pub version: i32,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    pub note: Option<String>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub applicant_name: Option<String>,
    pub has_cnc_program: bool,
    /// 工单已绑工序链且批次当前工序能在链内定位（判据见
    /// `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR`）。前端用它决定卡片的绿色边框。
    pub has_process_chain: bool,
}

/// 候选池行（`board_process_detail` SQL 4）。
pub struct PoolItemRow {
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub version: i32,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    pub note: Option<String>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub applicant_name: Option<String>,
    pub shelf_id: i64,
    pub shelf_code: String,
    pub shelf_name: String,
    pub has_cnc_program: bool,
    /// 同 [`HeldBatchRow::has_process_chain`]（同一常量、同一判据）。
    pub has_process_chain: bool,
}

// ---------------------------------------------------------------------------
// repo
// ---------------------------------------------------------------------------

/// 队列板聚合 SQL 入口（ZST）。
pub struct QueueBoardRepo;

impl QueueBoardRepo {
    /// 工序序列板数据。**固定 3 条 SQL**（与工序数无关）。
    ///
    /// 1. 各工序候选池计数（`GROUP BY current_process_id`）；
    /// 2. 工序元数据（`id = ANY($1)` 一次批量，零 N+1）；
    /// 3. 待下发批次数。
    ///
    /// ⚠️ 第 3 条是 VO 契约 `pending_count` 要求的（「待下发」tab 标签要用这个
    /// 数字）。它与第 1、2 条是**不同粒度**（前者按工单聚合、这条按待下发状态
    /// 聚合），无法折进同一条 SQL；「固定条数」的目的是消灭随行数增长的查询，
    /// 多一条常量查询不违反该目的。原先 `GET /pool/counts` 返回的 VO 里没有这个
    /// 字段，前端是另发一次 `GET /pool/pending` 拿 `total` 补出来的 —— 那是第
    /// 4 个 HTTP 请求。
    ///
    /// 只返 `count > 0` 的工序（沿用既有 `group_count_by_process_all_shelves`
    /// 口径：SQL 不产 0 行组）。「工序元数据查不到」的行（工序已软删）以
    /// `(deleted#{id})` 占位名返回，与既有 `pool_counts_all_shelves` 的防御
    /// 策略一致 —— 计数仍要显示，否则运营看不到「这批货压在谁的池子里」。
    pub async fn board_snapshot(
        conn: &mut PgConnection,
    ) -> Result<(Vec<ProcessCountRow>, Vec<ProcessMetaRow>, i64), sqlx::Error> {
        let count_rows = sqlx::query(SQL_POOL_COUNT_BY_PROCESS)
            .fetch_all(&mut *conn)
            .await?;
        let counts: Vec<ProcessCountRow> = count_rows
            .into_iter()
            .map(|r| ProcessCountRow {
                process_id: r.get("process_id"),
                count: r.get("cnt"),
            })
            .collect();

        let process_ids: Vec<i64> = counts.iter().map(|c| c.process_id).collect();
        let meta: Vec<ProcessMetaRow> = if process_ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query(SQL_PROCESS_META_BY_IDS)
                .bind(&process_ids)
                .fetch_all(&mut *conn)
                .await?
                .into_iter()
                .map(row_to_process_meta)
                .collect()
        };

        let pending: i64 = sqlx::query(SQL_COUNT_PENDING)
            .fetch_one(&mut *conn)
            .await?
            .get("cnt");

        Ok((counts, meta, pending))
    }

    /// 单工序队列板数据。**固定 4 条 SQL，与工人数 / 批次数无关**：
    ///
    /// 1. 工序元数据（单行）；
    /// 2. 该工序可用工人（工种 `max_held` 在同一条 SQL 的 JOIN 里一并取回 ——
    ///    工人查询本身已经 JOIN `t_work_type`，把它拆出去等于同一份数据取两次）；
    /// 3. **全部工人的持有批次一次取齐**（`current_holder_id = ANY($1)`）；
    /// 4. 该工序候选池。
    ///
    /// ⚠️ 第 3 条是本方法与旧 `GET /queue/{process_id}` + `GET /queue/state`
    /// 组合的本质区别：旧路径下前端要发 1（工序）+ 1（候选池）+ N（每工人一次
    /// state）个请求；现在是恒定 4 条。
    ///
    /// 「待下发」计数**不在本方法内查**（`QueueProcessBoardDetail` 没有该字段）：
    /// 它是工序无关的全局量，由 `board_snapshot` 的 `pending_count` 提供，
    /// 前端在单工序板上直接复用首帧那个数字，省掉每次下钻一次查询。
    ///
    /// 工序不存在 / 已软删 → `20801 BIZ_PROCESS_NOT_FOUND`（HTTP 404）。
    pub async fn board_process_detail(
        conn: &mut PgConnection,
        process_id: i64,
    ) -> Result<QueueBoardProcessData, AppError> {
        // 1. 工序元数据
        let process_row = sqlx::query(SQL_PROCESS_META_ONE)
            .bind(process_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process {process_id} 不存在"),
                )
            })?;
        let process = ProcessMetaRow {
            id: process_row.get("id"),
            code: process_row.get("code"),
            name: process_row.get("name"),
            color: process_row.get("color"),
            // 单工序详情不展示 category，填空串占位（`category` 是 `String`，
            // 而 `t_process.category` 有 CHECK 约束非空，故不存在「真的是空」
            // 的可能；这里只是让字段复用同一行结构而不必再写一个 struct）。
            category: String::new(),
        };

        // 2. 工人（含工种 max_held）
        let worker_rows = sqlx::query(SQL_WORKERS_BY_PROCESS)
            .bind(process_id)
            .fetch_all(&mut *conn)
            .await?;
        let workers: Vec<WorkerRow> = worker_rows
            .into_iter()
            .map(|r| WorkerRow {
                worker_id: r.get("worker_id"),
                name: r.get("worker_name"),
                badge_code: r.get("badge_code"),
                work_type_code: r.get("work_type_code"),
                max_held: r.get("max_held"),
            })
            .collect();

        // 3. 全部工人的持有批次（一次 ANY，无 N+1）
        let held: Vec<HeldBatchRow> = if workers.is_empty() {
            Vec::new()
        } else {
            let ids: Vec<i64> = workers.iter().map(|w| w.worker_id).collect();
            // ⚠️ `{has_process_chain}` 填的是**编译期常量** `HAS_PROCESS_CHAIN_EXPR`，
            // 用户输入（`ids` / `process_id`）一律走 bind ⇒ 注入面为 0，
            // `AssertSqlSafe` 包裹安全（口径同 `outsource::board::repo`）。
            let sql = AssertSqlSafe(
                SQL_HELD_BATCHES_BY_WORKERS.replace("{has_process_chain}", HAS_PROCESS_CHAIN_EXPR),
            );
            sqlx::query(sql)
                .bind(&ids)
                .fetch_all(&mut *conn)
                .await?
                .into_iter()
                .map(row_to_held_batch)
                .collect()
        };

        // 4. 候选池（同上：只拼编译期常量）
        let sql = AssertSqlSafe(
            SQL_POOL_ITEMS_BY_PROCESS.replace("{has_process_chain}", HAS_PROCESS_CHAIN_EXPR),
        );
        let items: Vec<PoolItemRow> = sqlx::query(sql)
            .bind(process_id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(row_to_pool_item)
            .collect();

        Ok(QueueBoardProcessData {
            process,
            workers,
            held,
            items,
        })
    }
}

/// `board_process_detail` 的 repo ↔ service 边界返回类型。
pub struct QueueBoardProcessData {
    pub process: ProcessMetaRow,
    pub workers: Vec<WorkerRow>,
    pub held: Vec<HeldBatchRow>,
    pub items: Vec<PoolItemRow>,
}

fn row_to_process_meta(r: sqlx::postgres::PgRow) -> ProcessMetaRow {
    ProcessMetaRow {
        id: r.get("id"),
        code: r.get("code"),
        name: r.get("name"),
        color: r.get("color"),
        category: r.get("category"),
    }
}

fn row_to_held_batch(r: sqlx::postgres::PgRow) -> HeldBatchRow {
    HeldBatchRow {
        holder_id: r.get("holder_id"),
        batch_id: r.get("batch_id"),
        part_id: r.get("part_id"),
        batch_no: r.get("batch_no"),
        quantity: r.get("quantity"),
        location: r.get("location"),
        version: r.get("version"),
        serial_no: r.get("serial_no"),
        name: r.get("name"),
        drawing_no: r.get("drawing_no"),
        system_delivery_date: r.get("system_delivery_date"),
        planned_delivery_date: r.get("planned_delivery_date"),
        is_urgent: r.get("is_urgent"),
        note: r.get("note"),
        customer_name: r.get("customer_name"),
        parent_customer_name: r.get("parent_customer_name"),
        applicant_name: r.get("applicant_name"),
        has_cnc_program: r.get("has_cnc_program"),
        has_process_chain: r.get("has_process_chain"),
    }
}

fn row_to_pool_item(r: sqlx::postgres::PgRow) -> PoolItemRow {
    PoolItemRow {
        batch_id: r.get("batch_id"),
        part_id: r.get("part_id"),
        batch_no: r.get("batch_no"),
        quantity: r.get("quantity"),
        version: r.get("version"),
        serial_no: r.get("serial_no"),
        name: r.get("name"),
        drawing_no: r.get("drawing_no"),
        system_delivery_date: r.get("system_delivery_date"),
        is_urgent: r.get("is_urgent"),
        note: r.get("note"),
        customer_name: r.get("customer_name"),
        parent_customer_name: r.get("parent_customer_name"),
        // `t_part.applicant_name` 是字符串列、`t_applicant` 是名字表：前端卡片要
        // 显示申请人**已录制的名字**，故优先取 JOIN 结果，回退到 part 上的
        // 原始字符串（申请人未录入时后者仍可用）。
        applicant_name: r
            .try_get::<Option<String>, _>("applicant_name_resolved")
            .ok()
            .flatten()
            .or_else(|| r.get("applicant_name")),
        shelf_id: r.get("shelf_id"),
        shelf_code: r.get("shelf_code"),
        shelf_name: r.get("shelf_name"),
        has_cnc_program: r.get("has_cnc_program"),
        has_process_chain: r.get("has_process_chain"),
    }
}
