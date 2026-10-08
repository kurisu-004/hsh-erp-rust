//! dashboard 域交期工单数据访问（SQL 真源）
//!
//! 三个方法共用一份状态白名单（`DELIVERY_STATUSES`）与同一口径列（系统交期），
//! 是为了让「逾期数」「交期面板」「柱状图」三处数字互相自洽。
//!
//! ## 行单位（2026-10-10 起全域统一为工单级）
//! - **逾期计数 = 工单级**：装配件算 1 条，`t_part` 侧用 `assembly_id IS NULL`
//!   排除子件（`t_assembly` 侧直接查装配件表本身）。
//! - **交期面板三桶 = 工单级**：`t_part`（排除子件）与 `t_assembly` 各出一行，
//!   装配件行**替换**其子件行（子件只作为装配件已交量的计算中间量）。
//! - **柱状图 = 件级**（唯一仍是件级的一处）：下钻抽屉与分桶都查 `t_part` 全表
//!   （含装配件子件），子件各算 1 件、装配件本身不出现。
//!
//! ⚠️ 面板三桶与逾期 KPI 的**行集合仍互斥**（`overdue_count` / `upcoming` / `overdue`
//! 要求「一件没交过」，`partial` 要求「交过一部分」），但**时间窗口不再互斥**：
//! `partial` 刻意无时间窗口（产品决议：「部分已交不应该限制时间范围」），故它覆盖
//! `< today` 区间上的一批工单 —— 这些工单既不进逾期 KPI、也不进 `overdue` 桶，
//! 只出现在 `partial` 里。「按交期切两块」的直觉读法在 `partial` 上不成立。
//!
//! ⚠️ 本文件全部走运行时 `sqlx::query`（与本域既有风格一致，不进 `.sqlx/` 离线缓存），
//! 而运行时 `query` **不校验占位符个数**：SQL 少写一个 `$n` 不会编译失败、也不报错，
//! 那个参数被静默忽略。改动本文件的 SQL 后必须重跑集成测试
//! `delivery_order_details_total_exceeds_items_when_truncated`（`LIMIT $3` 漏写会让
//! 截断失效）与 `system_delivery_orders_bucket_total_is_full_match_count`
//! （三桶的 `LIMIT` 漏写会让 `total` 与 `items.len()` 一起失守）——后者是本文件
//! 唯一的 `LIMIT` 占位符防线。
//!
//! ## 三桶排序的 tiebreaker（2026-10-10）
//! 三条主查询都是 `ORDER BY system_delivery_date ASC NULLS LAST, id ASC`。
//! 规格给的主排序键只有 `system_delivery_date ASC`，**`id ASC` 是必要的补充**：PG 的
//! `ORDER BY` 对并列行**不保证稳定**，而 `LIMIT 30` 会切在并列区中间 ⇒ 同一个库两次
//! 请求可能返回不同的 30 行，集成测试也会间歇性红。`id ASC` 让截断确定（雪花 id 单调
//! 递增，等价于「同交期取建单最早的 30 条」，与本域旧查询的 `is_urgent DESC, id ASC`
//! 同思路）。

use chrono::NaiveDate;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};

use crate::modules::dashboard::dto::DeliveryBasis;
use crate::modules::dashboard::vo::{
    DeliveryBucket, DeliveryOrderDetail, DeliveryOrderDetailOut, SystemDeliveryOrder,
    SystemDeliveryOrders,
};

/// 影响「未交付」判定的状态白名单（2026-10-07）。
///
/// 柱状图 `UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses` 里 top(4) + middle(2)
/// 正是这 6 态，bottom 层额外含 DELIVERED。
/// 2026-10-07 前端把那两块交期面板的 urgent / partial 判定改为服务端按
/// `delivered_quantity` 判定后，6 态在前端**只剩 `LAYERS[].statuses` 这一个镜像**
/// ——改本常量时只需核对这一处。同步关系与症状见 docs/api/dashboard.md §5。
pub const DELIVERY_STATUSES: [&str; 6] = [
    "PENDING",
    "PROGRAMMING",
    "IN_PROCESS",
    "OUTSOURCE",
    "INSPECTION",
    "READY_TO_SHIP",
];

/// 每个分桶的最大行数（upcoming / overdue / partial 三桶各一条独立上限）。
pub const DELIVERY_BUCKET_LIMIT: usize = 30;

/// 柱状图下钻抽屉的行上限（`total` 不受它截断，前端显示「共 N 件」）。
pub const DELIVERY_DETAIL_LIMIT: i64 = 200;

/// 逾期未交计数：`t_part`（工单级，排除装配件子件）+ `t_assembly`（装配件本身）。
///
/// `t_assembly` 侧复用同一份 `DELIVERY_STATUSES`，天然只命中其中 4 态
/// （`AssemblyStatus` 只有 7 态，**无 `PROGRAMMING` / `OUTSOURCE`**，见
/// `src/modules/assembly/statemachine.rs` 的 `can_transition_to` 迁移表）——这是有意的：
/// 白名单按 part 状态域取全集，装配件侧对不上的两个状态自然不参与，不需要第二份常量。
///
/// ## 已交过就排除（2026-10-10 新增，口径决策）
/// 两侧各加一个 `NOT EXISTS`：**只要交过一部分就不计入逾期 KPI**。`t_assembly` 侧的
/// 子件路径是 `NOT EXISTS (… t_part JOIN t_part_batch …)`。
///
/// ### 为什么保留 `status = ANY(DELIVERY_STATUSES)` 双守卫，不要删
/// 两个守卫是**互相独立**的两口径：`NOT EXISTS` 读的是**批次列**（`t_part_batch.status`，
/// 真源），`status ∈ 6 态` 读的是**派生列**（`t_part.status` / `t_assembly.status`，
/// min-progress 派生缓存，滞后窗口）。两条同时成立才计入，于是本 SQL 的谓词与
/// `SQL_ORDERS_OVERDUE` 的桶谓词**逐字相同** ⇒ KPI ↔ 面板严格对数。只留一条会有两种漂移：
/// - 只留 `NOT EXISTS`：派生滞后期内仍处 `IN_PROCESS` 的已交批次数会被重复计入；
/// - 只留 `status = ANY`：批次被追溯改成 `CANCELLED` 的工单会凭派生列被计入。
///
/// ### 为什么 `NOT EXISTS` 与面板侧的「已交量」判定等价（业务不变式）
/// 装配件**只能整套交付、不允许单独交子件**（产品不变式）。装配件总套数 N、交 k 套 ⇒
/// 每个子件 c 交 `k × c.quantity / N` 件，于是
/// `per_set(c) = (子件已交 × N) / NULLIF(c.quantity, 0) = k`，
/// `delivered_sets = MIN over c (k) = LEAST(k, N) = k`，
/// 故 `delivered_sets == 0 ⟺ k == 0 ⟺ 无任何子件被交付 ⟺ NOT EXISTS(子件有已交批次)`。
/// ⇒ 本 SQL（用 `NOT EXISTS`）与 `upcoming` / `overdue` 桶（用 `delivered_sets`）
/// 在**散件和装配件两侧都对齐**。
///
/// **不变式被破坏后的实际现象**（唯一破坏路径是
/// `POST /api/v2/prod/batches/{id}/deliver` —— `prod::batch::repo::sql::mark_batch_delivered`
/// 只查 `allowed_from: &["READY_TO_SHIP"]`，无装配件套数校验，能单独交子件；
/// 送货单路径经 `entry_max_sets` 闸门（`com::delivery_note::service::scan_entry`，
/// 错误码 21405）维持不变式。存量违规数据由人工清理）：
///
/// 三桶的归属判据是**本文件的 `NOT EXISTS` / `EXISTS`**（不是 `delivered_quantity` 是否为 0），
/// 故 KPI 与 `upcoming` / `overdue` / `partial` 的**行集合在破坏不变式时依然互斥**
/// （`NOT EXISTS` ⟹ `delivered_sets` 恒为 0，反向不成立）。真正会错位的是**展示值**：
/// 一个「子件 A 交满、子件 B 一件没交」的半套装配件会落 `partial` 桶（`EXISTS` 命中），
/// 而 `delivered_quantity` 是 min 公式给出的 **0 套** ⇒ 前端看到「部分已交 / 0 套」。
/// 若反过来把分桶挪回 Rust 按 `delivered_quantity > 0` 判定，才会出现真正的
/// 「KPI 不计但面板有行」反向差 —— **别那样改**，判据必须留在 SQL 里（理由见
/// `SQL_ORDERS_PARTIAL` 的 doc）。
///
/// ——两处谓词字面不同却语义等价，**不要**把其中任何一条当成漏判"修"掉。
///
/// ⚠️ **刻意不加** `NOT EXISTS (… DELIVERED 事件)` 守卫：派生状态滞后窗口只影响
/// 一次刷新，事件表兜底是过度设计。与 `statistics::repo::sql::count_overdue_undelivered`
/// 是**有意分叉**——后者服务生产统计页（前端 `OverviewTab.vue`）且有事件口径测试，
/// 两条 SQL 的语义（planned 口径 + 事件兜底）都保持原样。**注意那是另一份 SQL**，
/// 本文件这份的 `overdue_count` 口径是 system + 已交量守卫。
const SQL_COUNT_OVERDUE: &str = "SELECT COUNT(*)::bigint AS cnt FROM ( \
    SELECT p.id FROM t_part p \
    WHERE p.deleted_at IS NULL \
      AND p.assembly_id IS NULL \
      AND p.system_delivery_date IS NOT NULL \
      AND p.system_delivery_date < $1 \
      AND p.status = ANY($2::varchar[]) \
      AND NOT EXISTS (SELECT 1 FROM t_part_batch b \
                       WHERE b.part_id = p.id AND b.deleted_at IS NULL \
                         AND b.status IN ('DELIVERED', 'COMPLETED')) \
    UNION ALL \
    SELECT a.id FROM t_assembly a \
    WHERE a.deleted_at IS NULL \
      AND a.system_delivery_date IS NOT NULL \
      AND a.system_delivery_date < $1 \
      AND a.status = ANY($2::varchar[]) \
      AND NOT EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id \
                      WHERE c.assembly_id = a.id AND c.deleted_at IS NULL \
                        AND b.deleted_at IS NULL \
                        AND b.status IN ('DELIVERED', 'COMPLETED')) \
  ) s";

/// `upcoming` 桶：`sdd >= today` + 一件没交过，按 `sdd ASC`，取前 `DELIVERY_BUCKET_LIMIT` 条。
///
/// 谓词与 `SQL_COUNT_OVERDUE` 的 `t_part` / `t_assembly` 两段**逐字相同**（除 `< $1`
/// 改成 `>= $1`），故 KPI 与该桶不重不漏。占位符：`$1` = today、`$2` = 状态白名单、
/// `$3` = LIMIT（`$3` 漏写会让 `total` 与截断一起失守，见文件头）。
const SQL_ORDERS_UPCOMING: &str = "SELECT u.*, COUNT(*) OVER () AS total FROM ( \
    SELECT p.id, p.serial_no, p.name, p.quantity, p.status, p.system_delivery_date, \
           p.customer_id, p.is_urgent, 'PART'::text AS row_type \
    FROM t_part p \
    WHERE p.deleted_at IS NULL \
      AND p.assembly_id IS NULL \
      AND p.system_delivery_date >= $1 \
      AND p.status = ANY($2::varchar[]) \
      AND NOT EXISTS (SELECT 1 FROM t_part_batch b \
                       WHERE b.part_id = p.id AND b.deleted_at IS NULL \
                         AND b.status IN ('DELIVERED', 'COMPLETED')) \
    UNION ALL \
    SELECT a.id, a.serial_no, a.name, a.quantity, a.status, a.system_delivery_date, \
           a.customer_id, a.is_urgent, 'ASSEMBLY'::text AS row_type \
    FROM t_assembly a \
    WHERE a.deleted_at IS NULL \
      AND a.system_delivery_date >= $1 \
      AND a.status = ANY($2::varchar[]) \
      AND NOT EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id \
                      WHERE c.assembly_id = a.id AND c.deleted_at IS NULL \
                        AND b.deleted_at IS NULL \
                        AND b.status IN ('DELIVERED', 'COMPLETED')) \
  ) u \
  ORDER BY system_delivery_date ASC NULLS LAST, id ASC \
  LIMIT $3";

/// `overdue` 桶：`sdd < today` + 一件没交过，与 `SQL_COUNT_OVERDUE` 逐字同谓词
/// ⇒ `overdue.items.len() <= overdue_count`，且在「无已交批次」子集上二者相等
/// （差值只可能来自 `DELIVERY_BUCKET_LIMIT` 截断）。占位符同 `SQL_ORDERS_UPCOMING`。
const SQL_ORDERS_OVERDUE: &str = "SELECT u.*, COUNT(*) OVER () AS total FROM ( \
    SELECT p.id, p.serial_no, p.name, p.quantity, p.status, p.system_delivery_date, \
           p.customer_id, p.is_urgent, 'PART'::text AS row_type \
    FROM t_part p \
    WHERE p.deleted_at IS NULL \
      AND p.assembly_id IS NULL \
      AND p.system_delivery_date IS NOT NULL \
      AND p.system_delivery_date < $1 \
      AND p.status = ANY($2::varchar[]) \
      AND NOT EXISTS (SELECT 1 FROM t_part_batch b \
                       WHERE b.part_id = p.id AND b.deleted_at IS NULL \
                         AND b.status IN ('DELIVERED', 'COMPLETED')) \
    UNION ALL \
    SELECT a.id, a.serial_no, a.name, a.quantity, a.status, a.system_delivery_date, \
           a.customer_id, a.is_urgent, 'ASSEMBLY'::text AS row_type \
    FROM t_assembly a \
    WHERE a.deleted_at IS NULL \
      AND a.system_delivery_date IS NOT NULL \
      AND a.system_delivery_date < $1 \
      AND a.status = ANY($2::varchar[]) \
      AND NOT EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id \
                      WHERE c.assembly_id = a.id AND c.deleted_at IS NULL \
                        AND b.deleted_at IS NULL \
                        AND b.status IN ('DELIVERED', 'COMPLETED')) \
  ) u \
  ORDER BY system_delivery_date ASC NULLS LAST, id ASC \
  LIMIT $3";

/// `partial` 桶：交过一部分，**无时间窗口**（产品决议：「部分已交不应该限制时间范围。
/// 应该扫出全部的部分已交工单」），仍取前 `DELIVERY_BUCKET_LIMIT` 条、按 `sdd ASC`。
///
/// ⚠️ 本条**无窗口谓词**，故它的占位符编号比另两条整体前移一位：`$1` = 状态白名单、
/// `$2` = LIMIT（**没有** `$3`）。`system_delivery_date IS NULL` 的工单也在本桶内
/// （排序 `NULLS LAST`），这是有意的：既然不限时间范围，就不该把「没填交期」排除掉。
/// 已知偏差登记见 `docs/api/dashboard.md` §8.4。
///
/// 「交过一部分」用 `EXISTS`（与另两条的 `NOT EXISTS` 同一子查询取反）——**判据必须在
/// SQL 里**，拉全量回 Rust 分桶会让「前 30 条」的口径失真（截断发生在分桶之前）。
const SQL_ORDERS_PARTIAL: &str = "SELECT u.*, COUNT(*) OVER () AS total FROM ( \
    SELECT p.id, p.serial_no, p.name, p.quantity, p.status, p.system_delivery_date, \
           p.customer_id, p.is_urgent, 'PART'::text AS row_type \
    FROM t_part p \
    WHERE p.deleted_at IS NULL \
      AND p.assembly_id IS NULL \
      AND p.status = ANY($1::varchar[]) \
      AND EXISTS (SELECT 1 FROM t_part_batch b \
                  WHERE b.part_id = p.id AND b.deleted_at IS NULL \
                    AND b.status IN ('DELIVERED', 'COMPLETED')) \
    UNION ALL \
    SELECT a.id, a.serial_no, a.name, a.quantity, a.status, a.system_delivery_date, \
           a.customer_id, a.is_urgent, 'ASSEMBLY'::text AS row_type \
    FROM t_assembly a \
    WHERE a.deleted_at IS NULL \
      AND a.status = ANY($1::varchar[]) \
      AND EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id \
                  WHERE c.assembly_id = a.id AND c.deleted_at IS NULL \
                    AND b.deleted_at IS NULL \
                    AND b.status IN ('DELIVERED', 'COMPLETED')) \
  ) u \
  ORDER BY system_delivery_date ASC NULLS LAST, id ASC \
  LIMIT $2";

/// 柱状图下钻抽屉的主查询（planned 口径）。两口径各自是**完整字面量**，不做字符串
/// 拼接——拼列名会开注入面，且 SQL 文本不再可被静态检查 / 计划器友好解析。
/// 同时 SELECT 出两列交期（前端倒计列按自己的 basis 选列渲染）。
const SQL_DETAILS_PLANNED: &str = "SELECT id, serial_no, drawing_no, name, customer_id, status, \
         planned_delivery_date, system_delivery_date \
  FROM t_part \
  WHERE deleted_at IS NULL \
    AND planned_delivery_date = $1 \
    AND status = ANY($2::varchar[]) \
  ORDER BY planned_delivery_date ASC, id ASC \
  LIMIT $3";

/// 柱状图下钻抽屉的主查询（system 口径），同 `SQL_DETAILS_PLANNED` 只换交期列。
const SQL_DETAILS_SYSTEM: &str = "SELECT id, serial_no, drawing_no, name, customer_id, status, \
         planned_delivery_date, system_delivery_date \
  FROM t_part \
  WHERE deleted_at IS NULL \
    AND system_delivery_date = $1 \
    AND status = ANY($2::varchar[]) \
  ORDER BY system_delivery_date ASC, id ASC \
  LIMIT $3";

/// 同谓词的计数（无 LIMIT），供前端显示「共 N 件」。
const SQL_DETAILS_COUNT_PLANNED: &str = "SELECT COUNT(*)::bigint AS cnt \
  FROM t_part \
  WHERE deleted_at IS NULL \
    AND planned_delivery_date = $1 \
    AND status = ANY($2::varchar[])";

/// 同谓词的计数（system 口径）。
const SQL_DETAILS_COUNT_SYSTEM: &str = "SELECT COUNT(*)::bigint AS cnt \
  FROM t_part \
  WHERE deleted_at IS NULL \
    AND system_delivery_date = $1 \
    AND status = ANY($2::varchar[])";

pub struct DeliveryRepo;

impl DeliveryRepo {
    /// 逾期未交工单数（工单级；装配件算 1 条，子件不重复计入）。
    pub async fn count_overdue(
        conn: &mut PgConnection,
        today: NaiveDate,
    ) -> Result<i64, sqlx::Error> {
        let row = sqlx::query(SQL_COUNT_OVERDUE)
            .bind(today)
            .bind(DELIVERY_STATUSES)
            .fetch_one(conn)
            .await?;
        Ok(row.get::<i64, _>("cnt"))
    }

    /// 交期面板三桶：`upcoming`（`>= today` 未交）/ `overdue`（`< today` 未交）/
    /// `partial`（已交过一部分、无窗口）。
    ///
    /// 2026-10-10 起三条主查询各返 ≤`limit` 行、`COUNT(*) OVER ()` 顺带给出该桶的
    /// 匹配总数（`DeliveryBucket.total`，**不受 items 截断影响**）；桶归属、`NOT EXISTS`
    /// / `EXISTS` 判据、排序、截断**全部在 SQL 内**完成，不再有「拉全量回 Rust 分桶」。
    ///
    /// SQL 条数上界固定 **6 条**（3 条主查询 + 3 次批量聚合），**与命中行数无关** ——
    /// **禁止**逐行查客户名 / 已交量（那会让 90 行变 90~180 次查询）。三次聚合的入参集
    /// 为空时各跳过 1 条（故实际条数可能是 3~6，上界恒为 6）：
    /// 1. `SQL_ORDERS_UPCOMING` / 2. `SQL_ORDERS_OVERDUE` / 3. `SQL_ORDERS_PARTIAL`
    /// 4. `fetch_delivered_quantities`（三桶里 `row_type == 'PART'` 的全部 id）
    /// 5. `fetch_delivered_sets`（三桶里 `row_type == 'ASSEMBLY'` 的全部 id）
    /// 6. `fetch_customer_names`（三桶全部 `customer_id` 合并去重）
    pub async fn list_system_delivery_orders(
        conn: &mut PgConnection,
        today: NaiveDate,
        limit: usize,
    ) -> Result<SystemDeliveryOrders, sqlx::Error> {
        // `usize` 不是 sqlx 的可绑类型，三条主查询的 LIMIT 一律以 `i64` 绑定。
        let upcoming = sqlx::query(SQL_ORDERS_UPCOMING)
            .bind(today)
            .bind(DELIVERY_STATUSES)
            .bind(limit as i64)
            .fetch_all(&mut *conn)
            .await?;
        let overdue = sqlx::query(SQL_ORDERS_OVERDUE)
            .bind(today)
            .bind(DELIVERY_STATUSES)
            .bind(limit as i64)
            .fetch_all(&mut *conn)
            .await?;
        // partial 无窗口 ⇒ 占位符编号整体前移一位（状态白名单 `$1`、LIMIT `$2`），
        // 见 SQL_ORDERS_PARTIAL 的 doc。
        let partial = sqlx::query(SQL_ORDERS_PARTIAL)
            .bind(DELIVERY_STATUSES)
            .bind(limit as i64)
            .fetch_all(&mut *conn)
            .await?;

        let raw_upcoming = read_bucket(upcoming);
        let raw_overdue = read_bucket(overdue);
        let raw_partial = read_bucket(partial);

        // 三次聚合各扫一遍三桶的全部行（每行一次 push），命中行数只影响 push 次数、
        // 不影响 SQL 条数。
        let part_ids = raw_all(&[&raw_upcoming, &raw_overdue, &raw_partial])
            .filter(|r| r.row_type == ROW_TYPE_PART)
            .map(|r| r.id)
            .collect::<Vec<i64>>();
        let asm_ids = raw_all(&[&raw_upcoming, &raw_overdue, &raw_partial])
            .filter(|r| r.row_type == ROW_TYPE_ASSEMBLY)
            .map(|r| r.id)
            .collect::<Vec<i64>>();
        let delivered_parts = Self::fetch_delivered_quantities(conn, &part_ids).await?;
        let delivered_assemblies = Self::fetch_delivered_sets(conn, &asm_ids).await?;

        // 装配件的 `customer_id` 与散件同为 `t_customer.id`（雪花全局唯一）⇒ 同一张表
        // 同一次批量查即可，不必分两侧。`dedup_positive` 内部已剔除非正 id。
        let cust_ids = raw_all(&[&raw_upcoming, &raw_overdue, &raw_partial])
            .map(|r| r.customer_id)
            .collect::<Vec<i64>>();
        let cust_names = Self::fetch_customer_names(conn, &cust_ids).await?;

        Ok(SystemDeliveryOrders {
            upcoming: build_bucket(
                raw_upcoming,
                &delivered_parts,
                &delivered_assemblies,
                &cust_names,
            ),
            overdue: build_bucket(
                raw_overdue,
                &delivered_parts,
                &delivered_assemblies,
                &cust_names,
            ),
            partial: build_bucket(
                raw_partial,
                &delivered_parts,
                &delivered_assemblies,
                &cust_names,
            ),
        })
    }

    /// 柱状图下钻抽屉：单日 + 状态过滤的工单明细。
    pub async fn list_delivery_order_details(
        conn: &mut PgConnection,
        date: NaiveDate,
        statuses: &[&str],
        basis: DeliveryBasis,
        limit: i64,
    ) -> Result<DeliveryOrderDetailOut, sqlx::Error> {
        let (rows_sql, count_sql) = match basis {
            DeliveryBasis::Planned => (SQL_DETAILS_PLANNED, SQL_DETAILS_COUNT_PLANNED),
            DeliveryBasis::System => (SQL_DETAILS_SYSTEM, SQL_DETAILS_COUNT_SYSTEM),
        };

        let rows = sqlx::query(rows_sql)
            .bind(date)
            .bind(statuses)
            .bind(limit)
            .fetch_all(&mut *conn)
            .await?;
        let total: i64 = sqlx::query(count_sql)
            .bind(date)
            .bind(statuses)
            .fetch_one(&mut *conn)
            .await?
            .get("cnt");

        let cust_ids: Vec<i64> = rows
            .iter()
            .map(|r| r.get::<i64, _>("customer_id"))
            .collect();
        let (cust_names, l1_names) = Self::fetch_customer_two_level(conn, &cust_ids).await?;

        let items: Vec<DeliveryOrderDetail> = rows
            .into_iter()
            .map(|r| {
                let id: i64 = r.get("id");
                let customer_id: i64 = r.get("customer_id");
                DeliveryOrderDetail {
                    id: id.to_string(),
                    serial_no: r.try_get::<Option<String>, _>("serial_no").ok().flatten(),
                    drawing_no: r.try_get::<String, _>("drawing_no").unwrap_or_default(),
                    name: r.try_get::<String, _>("name").unwrap_or_default(),
                    l1_customer_name: l1_names.get(&customer_id).cloned(),
                    customer_name: cust_names.get(&customer_id).cloned(),
                    status: r.get::<String, _>("status"),
                    planned_delivery_date: r
                        .get::<NaiveDate, _>("planned_delivery_date")
                        .format("%Y-%m-%d")
                        .to_string(),
                    system_delivery_date: r
                        .try_get::<Option<NaiveDate>, _>("system_delivery_date")
                        .ok()
                        .flatten()
                        .map(|d| d.format("%Y-%m-%d").to_string()),
                }
            })
            .collect();

        Ok(DeliveryOrderDetailOut {
            date: date.format("%Y-%m-%d").to_string(),
            basis: match basis {
                DeliveryBasis::Planned => "planned".to_string(),
                DeliveryBasis::System => "system".to_string(),
            },
            total,
            items,
            ts: crate::infra::clock::now_shanghai_iso(),
        })
    }

    // =====================================================================
    // 私有批量查表（三处都用「一条 SQL 批量」而非逐行查，防 N+1）
    // =====================================================================

    /// 已送数量聚合（SQL 形态与 `part::service::list_enrichment::fetch_delivered_quantities`
    /// 一致：`::int` cast 不可省——PG 的 `SUM(int4)` 返 int8，直接绑 i32 会类型不匹配）。
    async fn fetch_delivered_quantities(
        conn: &mut PgConnection,
        part_ids: &[i64],
    ) -> Result<HashMap<i64, i32>, sqlx::Error> {
        let mut out: HashMap<i64, i32> = HashMap::new();
        if part_ids.is_empty() {
            return Ok(out);
        }
        let rows = sqlx::query(
            "SELECT part_id, COALESCE(SUM(quantity), 0)::int AS delivered_qty \
             FROM t_part_batch \
             WHERE part_id = ANY($1::bigint[]) AND deleted_at IS NULL \
               AND status IN ('DELIVERED', 'COMPLETED') \
             GROUP BY part_id",
        )
        .bind(part_ids)
        .fetch_all(&mut *conn)
        .await?;
        for r in rows {
            out.insert(r.get::<i64, _>("part_id"), r.get::<i32, _>("delivered_qty"));
        }
        Ok(out)
    }

    /// 装配件侧的「已交量」= **已交套数**（不是件数），口径是
    /// `LEAST(COALESCE(MIN(子件已送件数 × a.quantity / NULLIF(c.quantity, 0)), 0), a.quantity)`。
    ///
    /// **与 `part::service::list_enrichment::fetch_delivered_sets` 逐字同源**（同一公式、
    /// 同一组边界处理）。之所以必须复刻而不能直接调用：本域受
    /// `modules::dashboard::tests::dashboard_domain_depends_on_no_other_domain` 约束
    /// （`cargo test --lib` 强制扫 `src/modules/dashboard/**/*.rs`），禁止 import 他域
    /// 的任何实现。只读跨域聚合是本仓既定 pattern（同 `statistics` / `admin`），
    /// 于是这条 SQL 在 dashboard 域有自己的副本 —— **改动时两处必须同步**。
    ///
    /// 边界处理（逐条照抄自原实现的注释，因为每一条去掉都会静默算错）：
    /// - `COALESCE(SUM(...), 0)` 不可省：未交任何批次的子件若贡献 NULL 会被 `MIN` 忽略，
    ///   那样「子件 A 交一半、子件 B 一件没交」会误判成 A 能撑的套数；
    /// - `NULLIF(子件总量, 0)`：总量为 0 的子件让该项为 NULL 从而被 `MIN` 忽略
    ///   （不参与），既不整除零出错也不拖累 min；
    /// - `LEAST(COALESCE(MIN(...), 0), a.quantity)`：收口到工单总套数，同时兜住子件超交
    ///   （否则 UI 会出现「20 / 10 套」）。**`COALESCE` 必须在 `LEAST` 里面**：PG 的
    ///   `LEAST` 会忽略 NULL 实参（与 `MIN` 聚合同语义），写成
    ///   `COALESCE(LEAST(MIN(...), a.quantity), 0)` 会在「子件总量全为 0、`MIN` 为 NULL」
    ///   时返回 `a.quantity`（整套全交），与口径正好相反；
    /// - `LEAST` 顺带消除 int8→int4 收窄溢出（`子件已送 × 装配件套数` 是 int8 乘积，
    ///   `1e6 × 1e6 = 1e12` 超 int4 会让整页 500）；收口后上界是 `a.quantity`（int4），
    ///   `::int` 不再可能溢出。PART 侧的 `fetch_delivered_quantities` 保持纯 `::int`
    ///   不加钳制：那边只是 `SUM(quantity)` 不放大，无真实溢出路径。
    ///
    /// **无子件的装配件不产生结果行**（SQL 以子件表为驱动表），调用方必须
    /// `.copied().unwrap_or(0)` 兜底 —— 那会让无子件装配件的 `delivered_quantity = 0`，
    /// 落进 `upcoming` / `overdue` 桶，与现状 `count_overdue` 计它的行为一致，保持不变。
    ///
    /// ### 为什么它与 `SQL_COUNT_OVERDUE` 的 `NOT EXISTS` 等价
    /// 装配件**只能整套交付、不允许单独交子件**（业务不变式）。总套数 N、交 k 套 ⇒ 每个
    /// 子件 c 交 `k × c.quantity / N` 件 ⇒ `per_set(c) = (子件已交 × N) / NULLIF(c.quantity, 0) = k`
    /// ⇒ `delivered_sets = MIN over c (k) = LEAST(k, N) = k`
    /// ⇒ `delivered_sets == 0 ⟺ k == 0 ⟺ 无任何子件被交付 ⟺ NOT EXISTS(子件有已交批次)`。
    /// 所以「未交过」的两条判据字面不同、语义等价。**不要**把它们中的任何一条当成漏判
    /// "修"掉 —— 三桶的归属判据是 SQL 里的 `NOT EXISTS` / `EXISTS`（不是本函数的返回值），
    /// 本函数只负责填 `delivered_quantity` 这个展示值。
    /// 不变式被破坏时错位的是展示值（行落 `partial` 却显示 0 套），不是 KPI ↔ 面板的行集合；
    /// 完整推导 + 唯一破坏路径见 `SQL_COUNT_OVERDUE` 的 doc 与 `docs/api/dashboard.md` §4.4。
    ///
    /// SQL 条数：1 条，与桶行数无关（防 N+1 往返）；扫描量是 O(本页装配件的子件总数)。
    async fn fetch_delivered_sets(
        conn: &mut PgConnection,
        asm_ids: &[i64],
    ) -> Result<HashMap<i64, i32>, sqlx::Error> {
        let mut out: HashMap<i64, i32> = HashMap::new();
        if asm_ids.is_empty() {
            return Ok(out);
        }
        // 以 `t_part`（子件）为驱动表，走 `(assembly_id, ...)` 前缀索引，因此整段仍只 1 条
        // SQL。不写死索引名：`ix_t_part_assembly_id_status` 与 `ix_t_part_assembly_id` 同
        // 前缀，planner 可能选后者，钉死名字必过期。
        // `GROUP BY c.assembly_id, a.quantity`：套数是表达式的一部分，必须进 GROUP BY。
        // ⚠️ `c.id` / `c.quantity` **未**进 GROUP BY 却被 SELECT 表达式引用，靠的是
        // 「相关标量子查询的外层引用不受 grouping 检查」这一 PG 行为 —— 标准 SQL 应拒绝。
        // **把相关子查询改写成 LEFT JOIN 或改用窗口函数会立刻报**
        // `column "c.id" must appear in the GROUP BY clause`，改写前务必先跑
        // `tests/dashboard_ws_api.rs` 与 `tests/com/union_list.rs` 的 `delivered_quantity_*` 用例。
        let rows: Vec<(i64, i32)> = sqlx::query_as(
            "SELECT c.assembly_id, \
                    LEAST(COALESCE(MIN( \
                        (COALESCE((SELECT SUM(b.quantity) FROM t_part_batch b \
                                   WHERE b.part_id = c.id AND b.deleted_at IS NULL \
                                     AND b.status IN ('DELIVERED', 'COMPLETED')), 0) \
                             * a.quantity) / NULLIF(c.quantity, 0) \
                    ), 0), a.quantity)::int AS delivered_sets \
             FROM t_part c \
             JOIN t_assembly a ON a.id = c.assembly_id AND a.deleted_at IS NULL \
             WHERE c.assembly_id = ANY($1) AND c.deleted_at IS NULL \
             GROUP BY c.assembly_id, a.quantity",
        )
        .bind(asm_ids)
        .fetch_all(&mut *conn)
        .await?;
        for (asm_id, sets) in rows {
            out.insert(asm_id, sets);
        }
        Ok(out)
    }

    /// 客户 id → name（单级，足够填 `SystemDeliveryOrder.customer_name`）。
    async fn fetch_customer_names(
        conn: &mut PgConnection,
        cust_ids: &[i64],
    ) -> Result<HashMap<i64, String>, sqlx::Error> {
        let Some(ids) = dedup_positive(cust_ids) else {
            return Ok(HashMap::new());
        };
        let rows = sqlx::query("SELECT id, name FROM t_customer WHERE id = ANY($1::bigint[])")
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<i64, _>("id"), r.get::<String, _>("name")))
            .collect())
    }

    /// 客户两级批量查（叶子名 + L1 名），抽屉行需要 `l1_customer_name`。
    async fn fetch_customer_two_level(
        conn: &mut PgConnection,
        cust_ids: &[i64],
    ) -> Result<(HashMap<i64, String>, HashMap<i64, String>), sqlx::Error> {
        let Some(ids) = dedup_positive(cust_ids) else {
            return Ok((HashMap::new(), HashMap::new()));
        };
        let rows =
            sqlx::query("SELECT id, name, parent_id FROM t_customer WHERE id = ANY($1::bigint[])")
                .bind(&ids)
                .fetch_all(&mut *conn)
                .await?;
        let mut names: HashMap<i64, String> = HashMap::new();
        let mut parent_of: HashMap<i64, Option<i64>> = HashMap::new();
        let mut parent_ids: Vec<i64> = Vec::new();
        for r in rows {
            let id: i64 = r.get("id");
            names.insert(id, r.get::<String, _>("name"));
            let parent: Option<i64> = r.try_get("parent_id").ok().flatten();
            if let Some(pid) = parent {
                parent_ids.push(pid);
            }
            parent_of.insert(id, parent);
        }
        let Some(pids) = dedup_positive(&parent_ids) else {
            // 无 L1：两列都退化成叶子客户名，保持「L1 恒有值」的前端契约
            let l1 = names.clone();
            return Ok((names, l1));
        };
        let parent_rows =
            sqlx::query("SELECT id, name FROM t_customer WHERE id = ANY($1::bigint[])")
                .bind(pids)
                .fetch_all(&mut *conn)
                .await?;
        let parent_names: HashMap<i64, String> = parent_rows
            .into_iter()
            .map(|r| (r.get::<i64, _>("id"), r.get::<String, _>("name")))
            .collect();
        let l1_names: HashMap<i64, String> = parent_of
            .iter()
            .map(|(id, parent)| {
                let l1 = parent
                    .and_then(|pid| parent_names.get(&pid).cloned())
                    .or_else(|| names.get(id).cloned())
                    .unwrap_or_default();
                (*id, l1)
            })
            .collect();
        Ok((names, l1_names))
    }
}

/// 行来源字面量（与三桶主查询里的 `'PART'::text` / `'ASSEMBLY'::text` 逐字对齐）。
/// `row_type` 直接进 VO，所以用 `&'static str` 而非 `String`。
const ROW_TYPE_PART: &str = "PART";
const ROW_TYPE_ASSEMBLY: &str = "ASSEMBLY";

/// 主查询的原始行（已交量 / 客户名尚未回填 —— 那三项来自 3 次批量聚合，
/// 在 `build_bucket` 里按 `row_type` 分别接 `delivered_parts` / `delivered_assemblies`）。
struct RawOrder {
    id: i64,
    serial_no: Option<String>,
    name: String,
    quantity: i32,
    status: String,
    system_delivery_date: Option<NaiveDate>,
    customer_id: i64,
    is_urgent: bool,
    row_type: &'static str,
}

/// 单桶的原始形态：SQL 已经把行截到 `LIMIT`，`total` 由 `COUNT(*) OVER ()` 给出。
///
/// ⚠️ `total` 是**窗口函数**的结果，PG 在 `LIMIT` 之前算完 ⇒ 它是**匹配总数**而不是
/// 返回行数。零命中时**没有任何行**（窗口函数无从求值）⇒ `total` 退化为 0。
struct RawBucket {
    rows: Vec<RawOrder>,
    total: i64,
}

fn read_bucket(rows: Vec<PgRow>) -> RawBucket {
    let total = rows.first().map(|r| r.get::<i64, _>("total")).unwrap_or(0);
    let rows = rows
        .into_iter()
        .map(|r| RawOrder {
            id: r.get::<i64, _>("id"),
            serial_no: r.try_get::<Option<String>, _>("serial_no").ok().flatten(),
            name: r.try_get::<String, _>("name").unwrap_or_default(),
            quantity: r.get::<i32, _>("quantity"),
            status: r.get::<String, _>("status"),
            system_delivery_date: r
                .try_get::<Option<NaiveDate>, _>("system_delivery_date")
                .ok()
                .flatten(),
            customer_id: r.get::<i64, _>("customer_id"),
            is_urgent: r.get::<bool, _>("is_urgent"),
            // SQL 只投影这两个字面量；未知值一律按 PART 兜（真出现时 `delivered_*`
            // 两个 map 都查不到它 → delivered_quantity 退 0，至少不会 panic）。
            row_type: match r.get::<String, _>("row_type").as_str() {
                ROW_TYPE_ASSEMBLY => ROW_TYPE_ASSEMBLY,
                _ => ROW_TYPE_PART,
            },
        })
        .collect();
    RawBucket { rows, total }
}

/// 把三桶的行摊平成一个迭代器，供三次批量聚合各自过滤 / 收集入参 id。
fn raw_all<'a>(buckets: &'a [&'a RawBucket]) -> impl Iterator<Item = &'a RawOrder> {
    buckets.iter().flat_map(|b| b.rows.iter())
}

/// 回填已交量与客户名，产出对外的 `DeliveryBucket`。
///
/// 已交量按 `row_type` 取不同来源：PART 侧是**件数**，ASSEMBLY 侧是**套数**。
/// 两个 map 都以 id 为键，`unwrap_or(0)` 兜住「无子件的装配件」（`fetch_delivered_sets`
/// 不为它产生结果行，见该函数 doc）。
fn build_bucket(
    raw: RawBucket,
    delivered_parts: &HashMap<i64, i32>,
    delivered_assemblies: &HashMap<i64, i32>,
    cust_names: &HashMap<i64, String>,
) -> DeliveryBucket {
    let items = raw
        .rows
        .into_iter()
        .map(|r| {
            let delivered_quantity = if r.row_type == ROW_TYPE_ASSEMBLY {
                delivered_assemblies.get(&r.id).copied().unwrap_or(0)
            } else {
                delivered_parts.get(&r.id).copied().unwrap_or(0)
            };
            SystemDeliveryOrder {
                id: r.id.to_string(),
                serial_no: r.serial_no,
                name: r.name,
                quantity: r.quantity,
                status: r.status,
                system_delivery_date: r
                    .system_delivery_date
                    .map(|d| d.format("%Y-%m-%d").to_string()),
                customer_name: cust_names.get(&r.customer_id).cloned(),
                is_urgent: r.is_urgent,
                delivered_quantity,
                row_type: r.row_type,
            }
        })
        .collect();
    DeliveryBucket {
        items,
        total: raw.total,
    }
}

/// 去重并剔除非正 id（`t_part.customer_id` / `t_assembly.customer_id` 是逻辑外键，
/// 脏数据下可能是 0）。空集返回 `None`，让调用方跳过 SQL。
fn dedup_positive(ids: &[i64]) -> Option<Vec<i64>> {
    let set: HashSet<i64> = ids.iter().copied().filter(|id| *id > 0).collect();
    if set.is_empty() {
        None
    } else {
        Some(set.into_iter().collect())
    }
}
