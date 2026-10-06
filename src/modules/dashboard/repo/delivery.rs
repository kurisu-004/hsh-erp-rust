//! dashboard 域交期工单数据访问（SQL 真源）
//!
//! 三个方法共用一份状态白名单（`DELIVERY_STATUSES`）与同一口径列（系统交期），
//! 是为了让「逾期数」「最紧急面板」「柱状图」三处数字互相自洽。
//!
//! ## 行单位差异（务必登记，跨端对数时会踩）
//! - **逾期计数 = 工单级**：装配件算 1 条，`t_part` 侧用 `assembly_id IS NULL`
//!   排除子件（`t_assembly` 侧直接查装配件表本身）。
//! - **面板 / 柱状图 = 件级**：子件各算 1 件（`t_part` 全表，含装配件子件），
//!   装配件本身不出现。
//!
//! 两者不冲突：逾期窗口是 `< today`，面板 / 图的窗口是 `>= today`，**时间窗口不重叠**，
//! 同一条工单不会同时出现在两处。
//!
//! ⚠️ 本文件全部走运行时 `sqlx::query`（与本域既有风格一致，不进 `.sqlx/` 离线缓存），
//! 而运行时 `query` **不校验占位符个数**：SQL 少写一个 `$n` 不会编译失败、也不报错，
//! 那个参数被静默忽略。改动本文件的 SQL 后必须重跑集成测试
//! `delivery_order_details_total_exceeds_items_when_truncated`（`LIMIT $3` 漏写会让
//! 截断失效，正是该用例的守门目标）。

use chrono::NaiveDate;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};

use crate::modules::dashboard::dto::DeliveryBasis;
use crate::modules::dashboard::vo::{
    DeliveryOrderDetail, DeliveryOrderDetailOut, SystemDeliveryOrder, SystemDeliveryOrders,
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

/// 最紧急 / 部分已交面板的窗口天数。窗口边界与分桶截断都在服务端算，前端不过滤。
pub const DELIVERY_WINDOW_DAYS: i64 = 7;

/// 每个分桶的最大行数（urgent / partial 各一条独立上限）。
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
/// ⚠️ **刻意不加** `NOT EXISTS (… DELIVERED 事件)` 守卫：派生状态滞后窗口只影响
/// 一次刷新，事件表兜底是过度设计。与 `statistics::repo::sql::count_overdue_undelivered`
/// 是**有意分叉**——后者服务生产统计页（前端 `OverviewTab.vue`）且有事件口径测试，
/// 两条 SQL 的语义（planned 口径 + 事件兜底）都保持原样。
const SQL_COUNT_OVERDUE: &str = "SELECT COUNT(*)::bigint AS cnt FROM ( \
    SELECT id FROM t_part \
    WHERE deleted_at IS NULL \
      AND assembly_id IS NULL \
      AND system_delivery_date IS NOT NULL \
      AND system_delivery_date < $1 \
      AND status = ANY($2::varchar[]) \
    UNION ALL \
    SELECT id FROM t_assembly \
    WHERE deleted_at IS NULL \
      AND system_delivery_date IS NOT NULL \
      AND system_delivery_date < $1 \
      AND status = ANY($2::varchar[]) \
  ) s";

/// 最紧急 / 部分已交面板的主查询：`t_part` 全表（含装配件子件，无 `t_assembly`），
/// 窗口 `[today, today + DELIVERY_WINDOW_DAYS)`。
const SQL_SYSTEM_DELIVERY_ORDERS: &str = "SELECT id, serial_no, name, quantity, status, system_delivery_date, customer_id, is_urgent \
     FROM t_part \
     WHERE deleted_at IS NULL \
       AND system_delivery_date >= $1 \
       AND system_delivery_date < $2 \
       AND status = ANY($3::varchar[]) \
     ORDER BY system_delivery_date ASC, is_urgent DESC, id ASC";

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

    /// 最紧急工单（`delivered_quantity == 0`）+ 部分已交（`> 0`）两桶。
    ///
    /// SQL 条数固定 3 条（主查询 + 已送数量聚合 + 客户名批量），与命中行数无关 ——
    /// **禁止**逐行查客户名（那会让 100 行变 100~200 次查询）。
    pub async fn list_system_delivery_orders(
        conn: &mut PgConnection,
        today: NaiveDate,
        limit: usize,
    ) -> Result<SystemDeliveryOrders, sqlx::Error> {
        let window_end = today + chrono::Duration::days(DELIVERY_WINDOW_DAYS);
        let rows = sqlx::query(SQL_SYSTEM_DELIVERY_ORDERS)
            .bind(today)
            .bind(window_end)
            .bind(DELIVERY_STATUSES)
            .fetch_all(&mut *conn)
            .await?;

        let part_ids: Vec<i64> = rows.iter().map(|r| r.get::<i64, _>("id")).collect();
        let delivered = Self::fetch_delivered_quantities(conn, &part_ids).await?;
        let cust_ids: Vec<i64> = rows
            .iter()
            .map(|r| r.get::<i64, _>("customer_id"))
            .collect();
        let cust_names = Self::fetch_customer_names(conn, &cust_ids).await?;

        let mut urgent: Vec<SystemDeliveryOrder> = Vec::new();
        let mut partial: Vec<SystemDeliveryOrder> = Vec::new();
        for r in rows {
            let id: i64 = r.get("id");
            let customer_id: i64 = r.get("customer_id");
            let delivered_quantity = delivered.get(&id).copied().unwrap_or(0);
            let order = SystemDeliveryOrder {
                id: id.to_string(),
                serial_no: r.try_get::<Option<String>, _>("serial_no").ok().flatten(),
                name: r.try_get::<String, _>("name").unwrap_or_default(),
                quantity: r.get::<i32, _>("quantity"),
                status: r.get::<String, _>("status"),
                system_delivery_date: r
                    .try_get::<Option<NaiveDate>, _>("system_delivery_date")
                    .ok()
                    .flatten()
                    .map(|d| d.format("%Y-%m-%d").to_string()),
                customer_name: cust_names.get(&customer_id).cloned(),
                is_urgent: r.get::<bool, _>("is_urgent"),
                delivered_quantity,
            };
            // 服务端分桶：0 = 一件没交过（最紧急），> 0 = 已交过一部分。
            // 判定必须留在这里，前端不再自己按已交数量分桶。
            if delivered_quantity == 0 {
                urgent.push(order);
            } else {
                partial.push(order);
            }
        }
        // 两桶各自独立截断；行序由 SQL 的 `system_delivery_date ASC` 保证，
        // 截断取的是最早到期的一批。
        urgent.truncate(limit);
        partial.truncate(limit);
        Ok(SystemDeliveryOrders { urgent, partial })
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

/// 去重并剔除非正 id（`t_part.customer_id` 是逻辑外键，脏数据下可能是 0）。
/// 空集返回 `None`，让调用方跳过 SQL。
fn dedup_positive(ids: &[i64]) -> Option<Vec<i64>> {
    let set: HashSet<i64> = ids.iter().copied().filter(|id| *id > 0).collect();
    if set.is_empty() {
        None
    } else {
        Some(set.into_iter().collect())
    }
}
