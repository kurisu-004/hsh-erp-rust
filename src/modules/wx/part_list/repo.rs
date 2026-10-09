//! wx::part_list 子模块 repo 层 —— SQL 真源
//!
//! 2026-10-11 新增：自旧 `src/modules/wx/repo.rs` 的 `PartCounts` / `PartList`
//! 两个结构搬入本文件（旧文件只剩 batch / worker 相关的结构，见其模块 doc）。
//!
//! ## 结构
//! ZST `PartListRepo` + 2 个静态方法（`counts_by_status` / `list_parts`），收
//! `PgExecutor` 泛型、返 `sqlx::Error`（由 service 层映射 `AppError`）。不抽 trait
//! —— 两个只读查询没有需要 mock 的分支（沿 `prod::process_design::repo` 的取舍）。
//!
//! ## ★ WHERE 只写一份（2026-10-11 修「分页 bug」的结构性措施）
//! 旧 `repo.rs` 把同一段 WHERE 手抄两遍（`list` 一份、`count` 一份），改一处漏一处
//! 就让 `total` 与 `items` 对不上。本文件把 SELECT 列表 / FROM / WHERE / ORDER BY
//! 拆成 4 个私有常量，**唯一**的差异（`LIMIT` / `OFFSET` 两个绑定参数）在调用处拼。
//!
//! ## `?status=` 谓词：`= ANY($1::text[])`
//! 旧谓词是 `($1::text IS NULL OR p.status = $1::text)` —— 单值。本域的 tab 值映射
//! 允许 `delivered` 展开成 `['READY_TO_SHIP','DELIVERED']` 两个状态，故必须走数组。
//!
//! ## 保留 / 删除的 JOIN（2026-10-11）
//! **保留**：
//! - `LEFT JOIN t_customer c ON c.id = p.customer_id` —— 卡片要客户名
//! - `LEFT JOIN LATERAL (...) cb ON TRUE` —— 当前活跃批次的 `batch_no`
//!   （`kind=batch` 时用；多个活跃批次取 `batch_no ASC` 第一条）
//!
//! **删除**（2026-10-11）：`LEFT JOIN t_shelf sh` / `LEFT JOIN t_worker w` /
//! `LEFT JOIN t_outsource_company oc` 三条 JOIN + `COALESCE(sh.code, w.name,
//! oc.name) AS holder_label`。它们**只为** `current_holder_label` 一个字段而存在，
//! 而该字段前端从未读取。三条 JOIN 拖慢查询、还带一个「holder 多态歧义」的注释债
//! （见旧文件里 `prod::batch::repo::mod` 的对应说明），一并清掉。
//!
//! ## `deliveredQty` 子查询（2026-10-11 新增字段）
//! ```sql
//! COALESCE((SELECT SUM(b.quantity) FROM t_part_batch b
//!           WHERE b.part_id = p.id AND b.deleted_at IS NULL
//!             AND b.status IN ('DELIVERED','COMPLETED')), 0)::int
//! ```
//! 前端原先把 `deliveredQty` **硬编码为 0**（`services/parts.ts::toPartCardItem`），
//! 后端补真值。口径已实测（dev 库 1901 条工单全量）：`SUM(所有未软删批次
//! quantity) == t_part.quantity` 零例外；`DELIVERED` 桶 126/126 条
//! `deliveredQty == totalQty`；`IN_PROCESS` 桶有 5 条部分交付、`PENDING` 桶 1 条
//! ⇒ 该字段真的有信息量，不是恒 0。
//!
//! ⚠️ 这条子查询**只 SELECT**，与 CI 强制护栏 `no_outside_file_writes_batch_status`
//! 无关（该护栏只拦 `UPDATE t_part_batch SET status …`）。
//!
//! ## 排序
//! `ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, p.id ASC`
//! （紧急 + 交期近优先，与旧实现逐字一致）。
//!
//! ⚠️ `is_urgent` **只参与排序、不进 VO** —— 前端自己按 `dueDate` 在组件里算紧急度，
//! 从不读该字段。**不要**顺手把它从排序里删掉（删了会让小程序列表顺序突变）。

use sqlx::PgExecutor;

use super::model::PartListRow;

/// `wx::part_list` ZST 静态方法容器。
pub struct PartListRepo;

// ---- SQL 片段（4 个私有常量，`list_parts` 唯一拼装点）------------------------

/// SELECT 列表（与 [`PartListRow`] 字段一一对应，alias 名逐字对齐字段名）。
///
/// ⚠️ `current_batch_id` 进了 model 但**不进 VO**（2026-10-11 字段级移除，见
/// `docs/api/wx.md` §6）。保留该列是为了 model 仍是「SQL 投影的完整快照」，便于
/// 将来某个卡片变体要用批次 id 时不必改 SQL。
const SELECT_COLS: &str = "SELECT
    p.id                     AS id,
    p.serial_no              AS serial_no,
    p.name                   AS name,
    p.drawing_no             AS drawing_no,
    p.quantity               AS quantity,
    p.status                 AS status,
    p.planned_delivery_date  AS planned_delivery_date,
    p.assembly_id            AS assembly_id,
    c.name                   AS customer_name,
    cb.id                    AS current_batch_id,
    cb.batch_no              AS current_batch_no,
    COALESCE((
        SELECT SUM(b.quantity)
        FROM t_part_batch b
        WHERE b.part_id = p.id
          AND b.deleted_at IS NULL
          AND b.status IN ('DELIVERED', 'COMPLETED')
    ), 0)::int              AS delivered_qty";

/// FROM 子句（工单 + 客户名 + 当前活跃批次的 LATERAL）。
const FROM_SQL: &str = "
    FROM t_part p
    LEFT JOIN t_customer c ON c.id = p.customer_id
    LEFT JOIN LATERAL (
        SELECT id, batch_no
        FROM t_part_batch pb
        WHERE pb.part_id = p.id
          AND pb.deleted_at IS NULL
          AND pb.status NOT IN ('COMPLETED', 'CANCELLED')
        ORDER BY pb.batch_no ASC
        LIMIT 1
    ) cb ON TRUE";

/// WHERE 骨架（软删闸门 + tab 状态闸门）。
///
/// ⚠️ `$1::text[] IS NULL OR p.status = ANY($1::text[])`：`statuses` 传
/// `Option<&[&str]>`，`None` ⇒ PG 收到 NULL ⇒ 整条谓词为真（不过滤）。传
/// `Some(&["READY_TO_SHIP","DELIVERED"])` ⇒ 数组语义一次覆盖两个状态。
const WHERE_CLAUSE: &str = "
    WHERE p.deleted_at IS NULL
      AND ($1::text[] IS NULL OR p.status = ANY($1::text[]))";

/// ORDER BY 子句（服务端硬编码的「加急 + 交期近优先」）。
const ORDER_BY_CLAUSE: &str = " ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, p.id ASC";

impl PartListRepo {
    /// 各 DB 状态的工单计数（一次 `GROUP BY status` 拉全，由 service 归桶到 4 个 tab）。
    ///
    /// ⚠️ 刻意**不**接受 `statuses` 过滤：tab 角标恒是**全局**口径（小程序 4 个 tab
    /// 的数字是固定的，不会随当前筛选变）。用 `query!` 宏（编译期对库校验），
    /// 故本条查询的离线元数据在 `.sqlx/` 里。
    pub async fn counts_by_status<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"
            SELECT status AS "status!", COUNT(*) AS "cnt!"
            FROM t_part
            WHERE deleted_at IS NULL
            GROUP BY status
            "#,
        )
        .fetch_all(executor)
        .await?;
        Ok(rows.into_iter().map(|r| (r.status, r.cnt)).collect())
    }

    /// 工单卡片列表（两个端点共用的**唯一**列表查询路径）。
    ///
    /// - `statuses`：service 层归一化后的 DB 状态集；`None` = 不过滤
    /// - `limit`：调用方传 `size + 1`（`hasMore` 靠「取超一条」判定，**不额外打
    ///   count 查询**）
    /// - `offset`：`(page - 1) * size`
    ///
    /// 用**非宏** `sqlx::query_as`（行结构手写 `FromRow`）：WHERE 需要与
    /// `WHERE_CLAUSE` 共用一份常量，而 `query!` 宏只接受字面量 SQL（不能用
    /// `const` 标识符），共用就得把 WHERE 手抄第二遍 —— 那正是本文件要消灭的漂移。
    /// 与 `prod::process_design::repo` / `prod::scan::listing::repo` 的同形取舍一致。
    ///
    /// ⚠️ **注入面为 0**：`format!` 只填 4 个**编译期常量**
    /// （`SELECT_COLS` / `FROM_SQL` / `WHERE_CLAUSE` / `ORDER_BY_CLAUSE`），
    /// 三个入参（`statuses` / `limit` / `offset`）一律走 bind，故
    /// `AssertSqlSafe` 包裹安全 —— 与 `prod::scan::listing::repo::fetch_pickable`
    /// 的同一条理由。
    pub async fn list_parts<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: Option<&[&str]>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PartListRow>, sqlx::Error> {
        let sql =
            format!("{SELECT_COLS}{FROM_SQL}{WHERE_CLAUSE}{ORDER_BY_CLAUSE} LIMIT $2 OFFSET $3");
        sqlx::query_as::<_, PartListRow>(sqlx::AssertSqlSafe(sql))
            .bind(statuses)
            .bind(limit)
            .bind(offset)
            .fetch_all(executor)
            .await
    }
}
