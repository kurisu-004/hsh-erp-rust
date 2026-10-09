//! wx::part_list 子模块 repo 层 —— SQL 真源
//!
//! 2026-10-11 新增：自旧 `src/modules/wx/repo.rs` 的 `PartCounts` / `PartList`
//! 两个结构搬入本文件（旧文件只剩 batch / worker 相关的结构，见其模块 doc）。
//!
//! ## 结构
//! ZST `PartListRepo` + 3 个静态方法（`counts_by_status` / `count_null_date` /
//! `list_parts`），收 `PgExecutor` 泛型、返 `sqlx::Error`（由 service 层映射
//! `AppError`）。不抽 trait —— 只读查询没有需要 mock 的分支（沿
//! `prod::process_design::repo` 的取舍）。
//!
//! ## ★ WHERE 只写一份（2026-10-11 修「分页 bug」的结构性措施）
//! 旧 `repo.rs` 把同一段 WHERE 手抄两遍（`list` 一份、`count` 一份），改一处漏一处
//! 就让 `total` 与 `items` 对不上。本文件把 SELECT 列表 / FROM / WHERE 骨架 /
//! ORDER BY 拆成私有常量（日期谓词另拆 3 个片段），**唯一**的差异（`LIMIT` /
//! `OFFSET` 两个绑定参数 / 日期片段）在调用处拼。
//!
//! ## `?status=` 谓词：`= ANY($1::text[])`
//! 旧谓词是 `($1::text IS NULL OR p.status = $1::text)` —— 单值。本域的 tab 值映射
//! 允许 `inspecting` 展开成 `['INSPECTION','READY_TO_SHIP']` 两个状态，故必须走数组。
//!
//! 2026-10-12：`$1::text[] IS NULL OR` 那半截**删掉**了 —— `all` / 缺省也落到
//! 6 状态白名单，再没有「不过滤」这条路径，留着它只会让白名单被绕过。
//!
//! ## ★ 日期谓词拆成 3 个片段常量（2026-10-12 接上日期筛选）
//! 小程序 `date-nav-bar` 的日期此前是**纯装饰**的，本次把它接成真筛选，谓词打的是
//! `p.system_delivery_date`（**不是** `planned_delivery_date` —— 后者是「计划交期」，
//! 小程序日期栏展示与角标都对不上它）。
//!
//! 3 个片段与 `ignore_date` × `date` 的选装矩阵（`list_parts` 是唯一拼装点）：
//!
//! | `ignore_date` | `date` | 用哪个片段 | 效果 |
//! |---|---|---|---|
//! | `false` | `Some(d)` | [`DATE_SCOPE_EQ`] | `system_delivery_date = $4` |
//! | `false` | `None` | [`DATE_SCOPE_ABSENT`] | 无谓词（`$4::date IS NULL` 恒真） |
//! | `true` | 任意（忽略） | [`DATE_SCOPE_NULL`] | `system_delivery_date IS NULL` |
//!
//! ⚠️ **`$4` 只在 [`DATE_SCOPE_EQ`] / [`DATE_SCOPE_ABSENT`] 里出现**：PG 的
//! Parse 只认**引用到的最高**参数号，`ignore_date = true` 时 SQL 里根本没有 `$4`，
//! 多 bind 一个会让 PG 报 `bind message supplies 4 parameters, but prepared
//! statement requires 3`。故 [`PartListRepo::list_parts`] 用 `if !ignore_date`
//! 条件 bind —— 改这两处必须同改。
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

use chrono::NaiveDate;
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
///
/// ⚠️ 2026-10-12：`p.planned_delivery_date` → `p.system_delivery_date`。卡片
/// `dueDate` 必须与日期筛选谓词**同源**（否则「按某日筛选出来的卡片却写着另一个
/// 日期」），且 `system_delivery_date` 可空 —— 下方 WHERE 的日期片段同打这一列。
const SELECT_COLS: &str = "SELECT
    p.id                     AS id,
    p.serial_no              AS serial_no,
    p.name                   AS name,
    p.drawing_no             AS drawing_no,
    p.quantity               AS quantity,
    p.status                 AS status,
    p.system_delivery_date   AS system_delivery_date,
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

/// WHERE 骨架（软删闸门 + tab 状态闸门）。**不含**日期谓词（见 3 个日期片段）。
///
/// ⚠️ `p.status = ANY($1::text[])`：`statuses` 由 service 层从编译期常量表取出，
/// `inspecting` 一类 tab 一次覆盖两个状态。**没有** `Option` / `IS NULL OR` 分支 ——
/// 2026-10-12 起 `all` / 缺省也带 6 状态白名单，再没有「不过滤」这条路径。
const BASE_WHERE: &str = "
    WHERE p.deleted_at IS NULL
      AND p.status = ANY($1::text[])";

/// 日期片段 ①：`?date` 有值 ⇒ `system_delivery_date` 精确等于该日。
const DATE_SCOPE_EQ: &str = "
      AND p.system_delivery_date = $4::date";

/// 日期片段 ②：`?date` **缺省** ⇒ 无日期谓词。
///
/// 沿用本仓 `$n::T IS NULL` 惯用法（与旧的 `WHERE_CLAUSE` 的 `$1::text[] IS NULL`
/// 同一手法）：PG 收到 NULL 时整条谓词为真。**刻意**仍然引用 `$4` —— 这样
/// `ignore_date = false` 的两条分支参数个数一致，[`PartListRepo::list_parts`]
/// 只需按 `ignore_date` 一个开关条件 bind。
const DATE_SCOPE_ABSENT: &str = "
      AND $4::date IS NULL";

/// 日期片段 ③：`noSystemDate` tab ⇒ 只要 `system_delivery_date IS NULL`，与
/// `$4` **无关**（选了哪天都恒显示这一桶）。
///
/// ⚠️ 本片段**不引用** `$4`，故 `ignore_date = true` 时 SQL 只有 3 个参数位。
const DATE_SCOPE_NULL: &str = "
      AND p.system_delivery_date IS NULL";

/// ORDER BY 子句（服务端硬编码的「加急 + 交期近优先」）。
///
/// ⚠️ **不要**因为 2026-10-12 加了日期筛选就「顺手」把
/// `p.planned_delivery_date` 换成 `p.system_delivery_date`：**同一天内
/// `system_delivery_date` 是常量**（`noSystemDate` 那批全是 NULL），它做不了
/// 破平手。这一列仍是**日内**破平依据，删掉会让同一天的工单顺序退化成只按
/// `id ASC`（即雪花时间序），小程序列表顺序会突变。
const ORDER_BY_CLAUSE: &str = " ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, p.id ASC";

/// `&[&str]` → `Vec<String>`：两条 `query!` 宏把 `text[]` 参数定型成 `&[String]`，
/// 宏的 `MatchBorrow` 只支持顶层 `String → &str` 逐个借，**不支持** `&[&str]`
/// 整体通过（故调用处要写成 `&owned(statuses)`）。`list_parts` 走非宏
/// `query_as`，那里的 `&[&str]` 可以直接绑。
///
/// 代价是每次调用多 6 个短字符串的一次性堆分配 —— 纯读端点、每次 2 次调用，
/// 与一次 DB 往返相比可忽略。
fn owned(statuses: &[&str]) -> Vec<String> {
    statuses.iter().map(|s| (*s).to_string()).collect()
}

impl PartListRepo {
    /// 5 个「有日期」tab 的角标底数：按 `(status, system_delivery_date IS NULL)`
    /// 分组返回计数，交给 service 归桶。
    ///
    /// - `statuses`：恒为 6 状态白名单（角标**不随** `?status=` 变，见模块 doc）
    /// - `date`：`$2::date IS NULL OR system_delivery_date = $2::date` —— 缺省时
    ///   无谓词，于是 `is_null` 两组都会回来，由 service 决定要不要计入 dated 桶
    ///
    /// 用 `query!` 宏（编译期对库校验），故本条查询的离线元数据在 `.sqlx/` 里
    /// —— **SQL 一改必须重跑 `./scripts/sqlx_prepare.sh` 并提交新
    /// `.sqlx/query-*.json`**，否则 CI 的 `SQLX_OFFLINE=true cargo build` 会挂。
    pub async fn counts_by_status<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        date: Option<NaiveDate>,
    ) -> Result<Vec<(String, bool, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"
            SELECT status AS "status!",
                   (system_delivery_date IS NULL) AS "is_null!",
                   COUNT(*) AS "cnt!"
            FROM t_part
            WHERE deleted_at IS NULL
              AND status = ANY($1::text[])
              AND ($2::date IS NULL OR system_delivery_date = $2::date)
            GROUP BY status, (system_delivery_date IS NULL)
            "#,
            &owned(statuses),
            date
        )
        .fetch_all(executor)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.status, r.is_null, r.cnt))
            .collect())
    }

    /// `noSystemDate` tab 的角标：`statuses` 白名单里 `system_delivery_date IS NULL`
    /// 的行数。**与 `?date=` 完全无关**（选了哪天都恒显示这一桶）。
    ///
    /// ⚠️ 为什么不复用 [`PartListRepo::counts_by_status`] 的 `is_null = true` 组：
    /// 那条查询带 `$2::date IS NULL OR system_delivery_date = $2::date`，一旦
    /// `?date` 有值，NULL 行**根本不会出现在结果里**，`is_null` 组恒空。拆成两条
    /// 查询后各自单一职责、可独立断言，代价是多一次往返（本域只读、无事务，
    /// 与 `production` 域的两条标量 count 同款取舍）。
    pub async fn count_null_date<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
    ) -> Result<i64, sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT COUNT(*) AS "cnt!"
            FROM t_part
            WHERE deleted_at IS NULL
              AND status = ANY($1::text[])
              AND system_delivery_date IS NULL
            "#,
            &owned(statuses)
        )
        .fetch_one(executor)
        .await?;
        Ok(row.cnt)
    }

    /// 工单卡片列表（两个端点共用的**唯一**列表查询路径）。
    ///
    /// - `statuses`：service 层从编译期常量表取出的 DB 状态集（**非空**：2026-10-12
    ///   起 `all` 也是 6 状态白名单，没有「不过滤」这条路径）
    /// - `date`：`None` = 不加日期谓词；`Some(d)` = `system_delivery_date = d`
    /// - `ignore_date`：`true` = `noSystemDate` tab，**忽略** `date`，只取
    ///   `system_delivery_date IS NULL` 的行
    /// - `limit`：调用方传 `size + 1`（`hasMore` 靠「取超一条」判定，**不额外打
    ///   count 查询**）
    /// - `offset`：`(page - 1) * size`
    ///
    /// 用**非宏** `sqlx::query_as`（行结构手写 `FromRow`）：WHERE 需要与
    /// [`BASE_WHERE`] + 3 个日期片段常量共用一份，而 `query!` 宏只接受字面量 SQL
    /// （不能用 `const` 标识符），共用就得把 WHERE 手抄第二遍 —— 那正是本文件要
    /// 消灭的漂移。与 `prod::process_design::repo` / `prod::scan::listing::repo`
    /// 的同形取舍一致。
    ///
    /// ⚠️ **注入面为 0**：`format!` 只填**编译期常量**（`SELECT_COLS` / `FROM_SQL`
    /// / `BASE_WHERE` / 日期片段 / `ORDER_BY_CLAUSE`），4 个入参（`statuses` /
    /// `limit` / `offset` / `date`）一律走 bind，故 `AssertSqlSafe` 包裹安全 ——
    /// 与 `prod::scan::listing::repo::fetch_pickable` 的同一条理由。
    pub async fn list_parts<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        date: Option<NaiveDate>,
        ignore_date: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PartListRow>, sqlx::Error> {
        // 日期片段：`ignore_date` 优先（noSystemDate 忽略 date）
        let date_sql = if ignore_date {
            DATE_SCOPE_NULL
        } else if date.is_some() {
            DATE_SCOPE_EQ
        } else {
            DATE_SCOPE_ABSENT
        };
        let sql = format!(
            "{SELECT_COLS}{FROM_SQL}{BASE_WHERE}{date_sql}{ORDER_BY_CLAUSE} LIMIT $2 OFFSET $3"
        );
        let query = sqlx::query_as::<_, PartListRow>(sqlx::AssertSqlSafe(sql))
            .bind(statuses)
            .bind(limit)
            .bind(offset);
        // ⚠️ `DATE_SCOPE_NULL` 不引用 `$4`：此时 SQL 只有 3 个参数位，多 bind 一个
        // PG 会报 `bind message supplies 4 parameters, but prepared statement
        // requires 3`。条件 bind 与上面的片段选装是**同一个开关**。
        let query = if ignore_date { query } else { query.bind(date) };
        query.fetch_all(executor).await
    }
}
