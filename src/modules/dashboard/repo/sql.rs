//! dashboard 域数据访问（SQL 真源）
//!
//! ## 约定
//! - 全走运行时 `sqlx::query` / `sqlx::query_as`（与 statistics 域同 pattern，
//!   不依赖 query! 宏离线缓存——dashboard 聚合 SQL 字段多、不进 `.sqlx/`）
//!
//! ## 多表 JOIN 聚合方法签名
//! dashboard 的聚合方法每个内部包含 1~3 条 SQL，故签名收 `&mut PgConnection`
//! （与同域 service 方法同形），内部用 `&mut *conn` 多次 reborrow。trait impl 在
//! `repo/mod.rs` 借 `&mut **self` 转 `&mut PgConnection` 喂入。
//!
//! ## 聚合方法（snapshot_*）
//! - `snapshot_counters(today, days, basis)` —— 未来 N 天交付分桶（upcoming 柱状图）
//! - `count_inspection_batches()` —— 品检区待品检批次数
//! - `snapshot_recent_batches()` —— 工人在手 IN_PROCESS 批次
//! - `snapshot_workers(ids)` —— 工人 id → 名称 映射
//! - `snapshot_system_delivery_orders(today)` —— 交期面板三桶（在 `repo/delivery.rs`）
//!
//! 交期相关的逾期计数 / 面板 / 抽屉三个方法在 `repo/delivery.rs::DeliveryRepo`（SQL 真源
//! 独立成文件，便于与「状态白名单」常量同处一地）。

use chrono::NaiveDate;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, HashMap};

use crate::modules::dashboard::dto::DeliveryBasis;
use crate::modules::dashboard::vo::UpcomingDeliveryBucket;

/// t_part_batch 行精简（dashboard 聚合专用，无完整表行）
#[derive(Debug, Clone)]
pub struct BatchLite {
    pub batch_id: i64,
    pub holder_id: Option<i64>,
}

/// t_part 行精简（dashboard 聚合专用，无完整表行）
#[derive(Debug, Clone)]
pub struct PartLite {
    pub part_id: i64,
    pub serial_no: Option<String>,
    /// 批次量，取自 `t_part_batch.quantity AS quantity`（不是 `t_part.quantity`）
    pub quantity: i32,
    pub is_urgent: bool,
}

/// `snapshot_recent_batches` 聚合结果：工人在手的 IN_PROCESS 批次
pub struct RecentBatchesData {
    pub worker_pairs: Vec<(BatchLite, PartLite)>,
}

/// SQL 行 → (BatchLite, PartLite)
fn row_to_part_batch_pair(r: sqlx::postgres::PgRow) -> (BatchLite, PartLite) {
    (
        BatchLite {
            batch_id: r.get::<i64, _>("batch_id"),
            holder_id: r.try_get::<Option<i64>, _>("holder_id").ok().flatten(),
        },
        PartLite {
            part_id: r.get::<i64, _>("p_id"),
            serial_no: r.try_get::<Option<String>, _>("serial_no").ok().flatten(),
            quantity: r.get::<i32, _>("quantity"),
            is_urgent: r.get::<bool, _>("is_urgent"),
        },
    )
}

// ---------------------------------------------------------------------------
// `snapshot_counters` 的两段静态 SQL（2026-10-04 新增）
//
// 两口径同形，唯一差别是交期列：planned → `t_part.planned_delivery_date`，
// system → `t_part.system_delivery_date`。各自是**完整字面量**，不做字符串拼接
// （拼列名会开 SQL 注入面，且 SQL 文本不再可被静态检查 / 计划器友好解析）。
//
// WHERE 的两处范围比较对 NULL 恒为 false，故 `system_delivery_date IS NULL` 的
// 工单在 system 口径下整件不计入——与 union-list 端点「NULL 交期不被命中」的
// 既有语义一致，无需额外写 `IS NOT NULL`。
//
// ## 窗口下界必须来自形参而非 `CURRENT_DATE`（2026-10-07）
// 窗口下界取 `$2`（绑定 service 传入的 `today`），**不写 `CURRENT_DATE`**：
// `CURRENT_DATE` 是 DB **会话时区**的今天，与本仓统一的 Asia/Shanghai 口径
// （`infra::clock::now_naive()`）是两个独立时钟，测试容器会话时区正是 UTC。
// 两者不一致时（Shanghai 00:00–08:00 共 8 小时）`CURRENT_DATE == today - 1`，
// 于是 `today - 1` 那天命中的行落进一个**根本不生成**的桶被静默丢弃，
// 同时末桶恒为 0（SQL 窗口右开，右端随下界一起前移一天）。
// 形参化之后 SQL 窗口与 Rust 侧桶循环共用同一个 `today`，两个时钟只剩一个。
const SQL_COUNTERS_PLANNED: &str = "SELECT planned_delivery_date AS d, status AS s, COUNT(*)::bigint AS cnt \
     FROM t_part \
     WHERE deleted_at IS NULL \
       AND status NOT IN ('COMPLETED', 'CANCELLED') \
       AND planned_delivery_date >= $2::date \
       AND planned_delivery_date < $2::date + ($1::bigint || ' days')::interval \
     GROUP BY planned_delivery_date, status";

const SQL_COUNTERS_SYSTEM: &str = "SELECT system_delivery_date AS d, status AS s, COUNT(*)::bigint AS cnt \
     FROM t_part \
     WHERE deleted_at IS NULL \
       AND status NOT IN ('COMPLETED', 'CANCELLED') \
       AND system_delivery_date >= $2::date \
       AND system_delivery_date < $2::date + ($1::bigint || ' days')::interval \
     GROUP BY system_delivery_date, status";

// ---------------------------------------------------------------------------
// DashboardRepo（聚合静态方法 + 私有 SQL helper）
// ---------------------------------------------------------------------------

pub struct DashboardRepo;

impl DashboardRepo {
    // ──────────────────────────────────────────────────────────────────────
    // 1) snapshot_counters —— 未来 N 天交付分桶（COUNT + GROUP BY date+status）
    // ──────────────────────────────────────────────────────────────────────
    pub async fn snapshot_counters(
        conn: &mut PgConnection,
        today: NaiveDate,
        days: i64,
        basis: DeliveryBasis,
    ) -> Result<Vec<UpcomingDeliveryBucket>, sqlx::Error> {
        // 按 `date, status` 分组，让每个桶返回按 OrderStatus 细分的计数（柱状图分层
        // 堆叠底座）。WHERE 排除 COMPLETED / CANCELLED，故 by_status 不会含这两个 key
        // （沿前端 `z.record(z.string(), z.number())` 必填契约）。
        //
        // 两段完整字面量按 `basis` 二选一。`today` 由 service 传进来、**绑进 SQL 的
        // 窗口下界**（`$2`）而不是在 SQL 里取 `CURRENT_DATE`：桶序列的起点、
        // 响应 VO 的 `today` 字段、SQL 的窗口下界必须是同一个值。跨零点窗口内若 SQL
        // 另取一次 DB 会话时区的时钟，会与上面两个值差一天，表现为当天行被静默丢弃
        // 且末桶恒 0（详见两段常量上方的「窗口下界」小节）。
        let sql = match basis {
            DeliveryBasis::Planned => SQL_COUNTERS_PLANNED,
            DeliveryBasis::System => SQL_COUNTERS_SYSTEM,
        };
        let rows = sqlx::query(sql)
            .bind(days)
            .bind(today)
            .fetch_all(&mut *conn)
            .await?;
        // (date, status) → 件数
        let mut bucket: HashMap<(NaiveDate, String), i64> = HashMap::new();
        for r in rows {
            let d: NaiveDate = r.get("d");
            let s: String = r.get("s");
            let n: i64 = r.get("cnt");
            bucket.insert((d, s), n);
        }
        // 先按 date 二维 groupby → BTreeMap（按状态字母序），便于 JSON key 顺序确定
        let mut by_date: HashMap<NaiveDate, BTreeMap<String, i64>> = HashMap::new();
        for ((d, s), n) in bucket {
            by_date.entry(d).or_default().insert(s, n);
        }
        // 装配：固定 N 桶（today → today+N-1），缺失日期补空 by_status；count = 求和
        let mut out: Vec<UpcomingDeliveryBucket> = Vec::with_capacity(days as usize);
        for offset in 0..days {
            let d = today + chrono::Duration::days(offset);
            let by_status = by_date.remove(&d).unwrap_or_default();
            let count: i64 = by_status.values().sum();
            out.push(UpcomingDeliveryBucket {
                date: d.format("%Y-%m-%d").to_string(),
                count,
                by_status,
            });
        }
        Ok(out)
    }

    // ──────────────────────────────────────────────────────────────────────
    // 2) count_inspection_batches —— 品检区待品检批次数
    // ──────────────────────────────────────────────────────────────────────
    pub async fn count_inspection_batches(conn: &mut PgConnection) -> Result<i64, sqlx::Error> {
        // 只取 COUNT：前端「在检」KPI 只要一个数字，不渲染行内容。JOIN / 闸门条件与
        // 「品检区在持批次」原口径逐字保留（批次 status='INSPECTION' + 双软删闸门 +
        // holder ∈ 品检区 active 货架）。
        // 索引 `ix_t_part_batch_status_holder (status, current_holder_id)` 覆盖前两段。
        let row = sqlx::query(
            "WITH active_shelves AS ( \
                SELECT id FROM t_shelf \
                WHERE zone = 'INSPECTION' AND deleted_at IS NULL AND is_active = TRUE \
             ) \
             SELECT COUNT(*)::bigint AS cnt \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             WHERE b.status = 'INSPECTION' \
               AND b.deleted_at IS NULL \
               AND p.deleted_at IS NULL \
               AND b.current_holder_id IN (SELECT id FROM active_shelves)",
        )
        .fetch_one(conn)
        .await?;
        Ok(row.get::<i64, _>("cnt"))
    }

    // ──────────────────────────────────────────────────────────────────────
    // 3) snapshot_recent_batches —— 工人在手 IN_PROCESS 批次
    // ──────────────────────────────────────────────────────────────────────
    pub async fn snapshot_recent_batches(
        conn: &mut PgConnection,
    ) -> Result<RecentBatchesData, sqlx::Error> {
        let worker_pairs = Self::fetch_worker_rows(conn).await?;
        Ok(RecentBatchesData { worker_pairs })
    }

    // ──────────────────────────────────────────────────────────────────────
    // 4) snapshot_workers —— 工人 id → name 映射
    // ──────────────────────────────────────────────────────────────────────
    pub async fn snapshot_workers(
        conn: &mut PgConnection,
        ids: &[i64],
    ) -> Result<HashMap<i64, String>, sqlx::Error> {
        Self::fetch_worker_names(conn, ids).await
    }

    // =====================================================================
    // 私有 SQL helper（snapshot_recent_batches 内部复用）
    // =====================================================================

    /// 拉所有 IN_PROCESS + location=WORKER 的批次。
    ///
    /// `location = 'WORKER'` 是「在手加工」的语义边界（批次已从货架/品检区出池、
    /// 压在工人手上）。
    ///
    /// **不设总量上限、不做 per-holder 限流**：前端该列表没有分页/滚动加载，静默截流
    /// 只会让在制清单莫名缺行（且用户无从得知被截掉了多少）。工厂规模下全量返回的
    /// 行数可控。
    async fn fetch_worker_rows(
        conn: &mut PgConnection,
    ) -> Result<Vec<(BatchLite, PartLite)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT b.id            AS batch_id, \
                    b.current_holder_id AS holder_id, \
                    p.id           AS p_id, \
                    p.serial_no    AS serial_no, \
                    b.quantity     AS quantity, \
                    p.is_urgent    AS is_urgent \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             WHERE b.status = 'IN_PROCESS' \
               AND b.location = 'WORKER' \
               AND b.deleted_at IS NULL \
               AND p.deleted_at IS NULL \
               AND b.current_holder_id IS NOT NULL \
             ORDER BY b.current_holder_id ASC, \
                      p.is_urgent DESC, \
                      b.id ASC",
        )
        .fetch_all(conn)
        .await?;
        Ok(rows.into_iter().map(row_to_part_batch_pair).collect())
    }

    /// 拉工人 id → name 映射。
    async fn fetch_worker_names(
        conn: &mut PgConnection,
        ids: &[i64],
    ) -> Result<HashMap<i64, String>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query("SELECT id, name FROM t_worker WHERE id = ANY($1::bigint[])")
            .bind(ids)
            .fetch_all(conn)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<i64, _>("id"), r.get::<String, _>("name")))
            .collect())
    }
}
