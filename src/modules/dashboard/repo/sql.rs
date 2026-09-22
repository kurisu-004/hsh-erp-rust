//! dashboard 域数据访问（SQL 真源，零 diff 搬迁自 `service.rs`）
//!
//! 对应 Python myERP/repository/*（dashboard 聚合 SQL 散落多表 JOIN，无单仓文件）。
//!
//! ## 约定
//! - 全走运行时 `sqlx::query` / `sqlx::query_as`（与 statistics 域同 pattern，
//!   不依赖 query! 宏离线缓存——dashboard 聚合 SQL 字段多、不进 `.sqlx/`）
//! - 所有 SQL 字符串零 diff 从原 `service.rs` 全文搬迁（2026-09-22 Group E 重构）
//!
//! ## 多表 JOIN 聚合方法签名
//! dashboard 4 个聚合方法（`snapshot_*`）每个内部包含 1~5 条 SQL，故签名收
//! `&mut PgConnection`（与原 `service::build_snapshot_with_workers` 同形），
//! 内部用 `&mut *conn` 多次 reborrow。trait impl 在 `repo/mod.rs` 借
//! `&mut **self` 转 `&mut PgConnection` 喂入。
//!
//! ## 4 个聚合方法（snapshot_*）
//! - `snapshot_counters(days)` —— 未来 N 天交付分桶（upcoming_delivery_bucket）
//! - `snapshot_top_parts(top_n)` —— 产线架 + 品检区在持批次 + 客户 / 工序 名字查表
//! - `snapshot_recent_batches(top_n)` —— 工人持有 IN_PROCESS 批次 + 客户 / PICKED_UP 事件
//! - `snapshot_workers(ids)` —— 工人 id → 名称 映射（worker-held items 用）

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};

use crate::modules::dashboard::dto::UpcomingDeliveryBucket;

/// t_part_batch + t_part JOIN 行精简（dashboard 聚合专用，无完整表行）
///
/// 2026-09-15 review 修：原 `BatchLite` 缺 `batch_no` 字段导致 DTO 永远为 None。
/// 本结构从原 service.rs 全量平移，2026-09-22 重构不破坏契约。
#[derive(Debug, Clone)]
pub struct BatchLite {
    pub batch_id: i64,
    pub batch_no: Option<i32>,
    pub holder_id: Option<i64>,
    pub next_process_id: Option<i64>,
}

/// t_part 行精简（dashboard 聚合专用，无完整表行）
#[derive(Debug, Clone)]
pub struct PartLite {
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub planned_delivery_date: Option<NaiveDate>,
    pub customer_id: i64,
}

/// `snapshot_top_parts` 聚合结果：产线架 + IN_PROCESS 批次 + 品检区批次 + 客户 / 工序 名字
pub struct TopPartsData {
    /// 全部 active PRODUCTION 货架 (id, code, name)
    pub active_prod_shelves: Vec<(i64, String, String)>,
    /// 产线架上的 IN_PROCESS 批次（holder_id IN active_prod_ids）
    pub on_prod_pairs: Vec<(BatchLite, PartLite)>,
    /// 品检区扁平批次
    pub on_insp_pairs: Vec<(BatchLite, PartLite)>,
    /// 客户 id → (name, "parent / name" 路径)
    pub cust_map: HashMap<i64, (Option<String>, String)>,
    /// process id → name
    pub process_map: HashMap<i64, String>,
}

/// `snapshot_recent_batches` 聚合结果：工人持有 + 客户路径 + PICKED_UP 时间
pub struct RecentBatchesData {
    /// 工人持有的 IN_PROCESS 批次
    pub worker_pairs: Vec<(BatchLite, PartLite)>,
    /// 客户 id → (name, "parent / name" 路径)
    pub cust_map: HashMap<i64, (Option<String>, String)>,
    /// batch_id → 最近一次 PICKED_UP 时间戳
    pub picked_at_map: HashMap<i64, NaiveDateTime>,
}

/// SQL 行 → (BatchLite, PartLite)
fn row_to_part_batch_pair(r: sqlx::postgres::PgRow) -> (BatchLite, PartLite) {
    (
        BatchLite {
            batch_id: r.get::<i64, _>("batch_id"),
            batch_no: r.try_get::<Option<i32>, _>("batch_no").ok().flatten(),
            holder_id: r.try_get::<Option<i64>, _>("holder_id").ok().flatten(),
            next_process_id: r
                .try_get::<Option<i64>, _>("next_process_id")
                .ok()
                .flatten(),
        },
        PartLite {
            part_id: r.get::<i64, _>("p_id"),
            serial_no: r.try_get::<Option<String>, _>("serial_no").ok().flatten(),
            name: r.try_get::<String, _>("p_name").unwrap_or_default(),
            drawing_no: r.try_get::<String, _>("drawing_no").unwrap_or_default(),
            quantity: r.get::<i32, _>("quantity"),
            is_urgent: r.get::<bool, _>("is_urgent"),
            planned_delivery_date: r
                .try_get::<Option<NaiveDate>, _>("planned_delivery_date")
                .ok()
                .flatten(),
            customer_id: r.get::<i64, _>("customer_id"),
        },
    )
}

// ---------------------------------------------------------------------------
// DashboardRepo（4 个聚合静态方法 + 私有 SQL helper）
// ---------------------------------------------------------------------------

pub struct DashboardRepo;

impl DashboardRepo {
    // ──────────────────────────────────────────────────────────────────────
    // 1) snapshot_counters —— 未来 N 天交付分桶（COUNT + GROUP BY）
    // ──────────────────────────────────────────────────────────────────────
    pub async fn snapshot_counters(
        conn: &mut PgConnection,
        days: i64,
    ) -> Result<Vec<UpcomingDeliveryBucket>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT planned_delivery_date AS d, COUNT(*)::bigint AS cnt \
             FROM t_part \
             WHERE deleted_at IS NULL \
               AND status NOT IN ('COMPLETED', 'CANCELLED') \
               AND planned_delivery_date >= CURRENT_DATE \
               AND planned_delivery_date < CURRENT_DATE + ($1::bigint || ' days')::interval \
             GROUP BY planned_delivery_date",
        )
        .bind(days)
        .fetch_all(&mut *conn)
        .await?;
        let mut bucket: HashMap<NaiveDate, i64> = HashMap::new();
        for r in rows {
            let d: NaiveDate = r.get("d");
            let n: i64 = r.get("cnt");
            bucket.insert(d, n);
        }
        let today = chrono::Local::now().date_naive();
        let mut out: Vec<UpcomingDeliveryBucket> = Vec::new();
        for offset in 0..days {
            let d = today + chrono::Duration::days(offset);
            out.push(UpcomingDeliveryBucket {
                date: d.format("%Y-%m-%d").to_string(),
                count: *bucket.get(&d).unwrap_or(&0),
            });
        }
        Ok(out)
    }

    // ──────────────────────────────────────────────────────────────────────
    // 2) snapshot_top_parts —— 产线架 + IN_PROCESS 批次 + 品检区批次 + 客户 / 工序 名字
    // ──────────────────────────────────────────────────────────────────────
    pub async fn snapshot_top_parts(
        conn: &mut PgConnection,
        top_n: i64,
    ) -> Result<TopPartsData, sqlx::Error> {
        // 1) 所有 active 生产区货架
        let prod_shelves = sqlx::query(
            "SELECT id, code, name \
             FROM t_shelf \
             WHERE zone = 'PRODUCTION' AND deleted_at IS NULL AND is_active = TRUE \
             ORDER BY code ASC",
        )
        .fetch_all(&mut *conn)
        .await?;
        let active_prod_ids: Vec<i64> =
            prod_shelves.iter().map(|r| r.get::<i64, _>("id")).collect();

        // 2) 生产区 IN_PROCESS 批次 + 所属 part
        let on_prod_rows = if active_prod_ids.is_empty() {
            Vec::new()
        } else {
            let rows = sqlx::query(
                // 2026-09-16 PR-3 批次 step 化：next_process_id 列已删，JOIN step 取 process_id
                "SELECT b.id            AS batch_id, \
                        b.part_id       AS part_id, \
                        b.batch_no      AS batch_no, \
                        b.quantity      AS quantity, \
                        b.current_holder_id AS holder_id, \
                        s.process_id      AS next_process_id, \
                        p.id           AS p_id, \
                        p.serial_no    AS serial_no, \
                        p.name         AS p_name, \
                        p.drawing_no   AS drawing_no, \
                        p.is_urgent    AS is_urgent, \
                        p.planned_delivery_date AS planned_delivery_date, \
                        p.customer_id  AS customer_id \
                 FROM t_part_batch b \
                 JOIN t_part p ON p.id = b.part_id \
                 LEFT JOIN t_process_chain_step s ON s.id = b.current_process_step_id \
                 WHERE b.status = 'IN_PROCESS' \
                   AND b.deleted_at IS NULL \
                   AND p.deleted_at IS NULL \
                   AND b.current_holder_id = ANY($1::bigint[]) \
                 ORDER BY b.current_holder_id ASC, \
                          p.is_urgent DESC, \
                          p.planned_delivery_date ASC, \
                          b.id ASC",
            )
            .bind(&active_prod_ids)
            .fetch_all(&mut *conn)
            .await?;
            rows.into_iter().map(row_to_part_batch_pair).collect()
        };

        // 3) 品检区扁平（INSPECTION zone + INSPECTION status）
        let on_insp_rows =
            Self::fetch_zone_rows(&mut *conn, "INSPECTION", Some("INSPECTION"), top_n).await?;

        // 4) 批取客户路径（产线 + 品检 合并）
        let cust_ids: Vec<i64> = on_prod_rows
            .iter()
            .chain(on_insp_rows.iter())
            .map(|(_, p)| p.customer_id)
            .filter(|id| *id > 0)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let cust_map = Self::fetch_customer_path(&mut *conn, &cust_ids).await?;

        // 5) 批取工序名（产线 + 品检 合并）
        let mut process_ids: Vec<i64> = Vec::new();
        for (b, _) in on_prod_rows.iter().chain(on_insp_rows.iter()) {
            if let Some(np) = b.next_process_id {
                process_ids.push(np);
            }
        }
        process_ids.sort();
        process_ids.dedup();
        let process_map = Self::fetch_process_names(&mut *conn, &process_ids).await?;

        let active_prod_shelves: Vec<(i64, String, String)> = prod_shelves
            .into_iter()
            .map(|r| {
                (
                    r.get::<i64, _>("id"),
                    r.get::<String, _>("code"),
                    r.get::<String, _>("name"),
                )
            })
            .collect();

        Ok(TopPartsData {
            active_prod_shelves,
            on_prod_pairs: on_prod_rows,
            on_insp_pairs: on_insp_rows,
            cust_map,
            process_map,
        })
    }

    // ──────────────────────────────────────────────────────────────────────
    // 3) snapshot_recent_batches —— 工人持有 IN_PROCESS + 客户路径 + PICKED_UP 时间
    // ──────────────────────────────────────────────────────────────────────
    pub async fn snapshot_recent_batches(
        conn: &mut PgConnection,
        top_n: i64,
    ) -> Result<RecentBatchesData, sqlx::Error> {
        // 1) 工人持有的 IN_PROCESS 批次（location = 'WORKER'）
        let worker_pairs = Self::fetch_worker_rows(&mut *conn, top_n).await?;

        // 2) 批取客户路径
        let cust_ids: Vec<i64> = worker_pairs
            .iter()
            .map(|(_, p)| p.customer_id)
            .filter(|id| *id > 0)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let cust_map = Self::fetch_customer_path(&mut *conn, &cust_ids).await?;

        // 3) 批取 PICKED_UP 最近时间
        let worker_batch_ids: Vec<i64> =
            worker_pairs.iter().map(|(b, _)| b.batch_id).collect();
        let picked_at_map =
            Self::fetch_picked_up_at_map(&mut *conn, &worker_batch_ids).await?;

        Ok(RecentBatchesData {
            worker_pairs,
            cust_map,
            picked_at_map,
        })
    }

    // ──────────────────────────────────────────────────────────────────────
    // 4) snapshot_workers —— 工人 id → name 映射
    // ──────────────────────────────────────────────────────────────────────
    pub async fn snapshot_workers(
        conn: &mut PgConnection,
        ids: &[i64],
    ) -> Result<HashMap<i64, String>, sqlx::Error> {
        Self::fetch_worker_names(&mut *conn, ids).await
    }

    // =====================================================================
    // 私有 SQL helper（snapshot_top_parts / snapshot_recent_batches 内部复用）
    // =====================================================================

    /// 拉指定 zone 货架上的 IN_PROCESS（生产）或 INSPECTION（品检）批次。
    async fn fetch_zone_rows(
        conn: &mut PgConnection,
        zone: &str,
        status: Option<&str>,
        _top_n: i64,
    ) -> Result<Vec<(BatchLite, PartLite)>, sqlx::Error> {
        let status_value = status.unwrap_or("IN_PROCESS");
        let rows = sqlx::query(
            // 2026-09-16 PR-3 批次 step 化：next_process_id 列已删，JOIN step 取 process_id
            "WITH active_shelves AS ( \
                SELECT id FROM t_shelf \
                WHERE zone = $1 AND deleted_at IS NULL AND is_active = TRUE \
             ) \
             SELECT b.id            AS batch_id, \
                    b.part_id       AS part_id, \
                    b.batch_no      AS batch_no, \
                    b.quantity      AS quantity, \
                    b.current_holder_id AS holder_id, \
                    s.process_id      AS next_process_id, \
                    p.id           AS p_id, \
                    p.serial_no    AS serial_no, \
                    p.name         AS p_name, \
                    p.drawing_no   AS drawing_no, \
                    p.is_urgent    AS is_urgent, \
                    p.planned_delivery_date AS planned_delivery_date, \
                    p.customer_id  AS customer_id \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             LEFT JOIN t_process_chain_step s ON s.id = b.current_process_step_id \
             WHERE b.status = $2 \
               AND b.deleted_at IS NULL \
               AND p.deleted_at IS NULL \
               AND b.current_holder_id IN (SELECT id FROM active_shelves) \
             ORDER BY b.current_holder_id ASC, \
                      p.is_urgent DESC, \
                      p.planned_delivery_date ASC, \
                      b.id ASC",
        )
        .bind(zone)
        .bind(status_value)
        .fetch_all(conn)
        .await?;
        Ok(rows.into_iter().map(row_to_part_batch_pair).collect())
    }

    /// 拉所有 IN_PROCESS + location=WORKER 的批次（按 holder_id 分桶限流）。
    async fn fetch_worker_rows(
        conn: &mut PgConnection,
        top_n: i64,
    ) -> Result<Vec<(BatchLite, PartLite)>, sqlx::Error> {
        let rows = sqlx::query(
            // 2026-09-16 PR-3 批次 step 化：next_process_id 列已删，JOIN step 取 process_id
            "SELECT b.id            AS batch_id, \
                    b.part_id       AS part_id, \
                    b.batch_no      AS batch_no, \
                    b.quantity      AS quantity, \
                    b.current_holder_id AS holder_id, \
                    s.process_id      AS next_process_id, \
                    p.id           AS p_id, \
                    p.serial_no    AS serial_no, \
                    p.name         AS p_name, \
                    p.drawing_no   AS drawing_no, \
                    p.is_urgent    AS is_urgent, \
                    p.planned_delivery_date AS planned_delivery_date, \
                    p.customer_id  AS customer_id \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             LEFT JOIN t_process_chain_step s ON s.id = b.current_process_step_id \
             WHERE b.status = 'IN_PROCESS' \
               AND b.location = 'WORKER' \
               AND b.deleted_at IS NULL \
               AND p.deleted_at IS NULL \
               AND b.current_holder_id IS NOT NULL \
             ORDER BY b.current_holder_id ASC, \
                      p.is_urgent DESC, \
                      p.planned_delivery_date ASC, \
                      b.id ASC",
        )
        .fetch_all(conn)
        .await?;

        let pairs: Vec<(BatchLite, PartLite)> =
            rows.into_iter().map(row_to_part_batch_pair).collect();

        // 按 holder 分桶限流
        let mut per_holder: HashMap<i64, i64> = HashMap::new();
        let mut capped: Vec<(BatchLite, PartLite)> = Vec::new();
        for p in pairs {
            if let Some(h) = p.0.holder_id {
                let c = per_holder.entry(h).or_insert(0);
                *c += 1;
                if *c > top_n {
                    continue;
                }
            }
            capped.push(p);
        }
        Ok(capped)
    }

    /// 拉客户 id → (name, "parent / name" 路径)，支持二级 parent 查表。
    async fn fetch_customer_path(
        conn: &mut PgConnection,
        cust_ids: &[i64],
    ) -> Result<HashMap<i64, (Option<String>, String)>, sqlx::Error> {
        if cust_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let customers =
            sqlx::query("SELECT id, name, parent_id FROM t_customer WHERE id = ANY($1::bigint[])")
                .bind(cust_ids)
                .fetch_all(&mut *conn)
                .await?;
        let mut id_to_parent: HashMap<i64, Option<i64>> = HashMap::new();
        let mut id_to_name: HashMap<i64, String> = HashMap::new();
        let mut parent_ids: Vec<i64> = Vec::new();
        for r in customers {
            let id: i64 = r.get("id");
            let name: String = r.get("name");
            let parent: Option<i64> = r.try_get("parent_id").ok().flatten();
            id_to_parent.insert(id, parent);
            id_to_name.insert(id, name);
            if let Some(p) = parent {
                parent_ids.push(p);
            }
        }
        parent_ids.sort();
        parent_ids.dedup();
        let parents: Vec<(i64, String)> = if parent_ids.is_empty() {
            Vec::new()
        } else {
            let rows = sqlx::query("SELECT id, name FROM t_customer WHERE id = ANY($1::bigint[])")
                .bind(&parent_ids)
                .fetch_all(conn)
                .await?;
            rows.into_iter()
                .map(|r| (r.get::<i64, _>("id"), r.get::<String, _>("name")))
                .collect()
        };
        let parent_map: HashMap<i64, String> = parents.into_iter().collect();

        let mut out: HashMap<i64, (Option<String>, String)> = HashMap::new();
        for (id, name) in id_to_name.iter() {
            let parent_name = id_to_parent
                .get(id)
                .and_then(|p| p.and_then(|pid| parent_map.get(&pid).cloned()));
            let path = match (parent_name, name.as_str()) {
                (Some(p), c) if !p.is_empty() && !c.is_empty() => Some(format!("{p} / {c}")),
                (_, c) if !c.is_empty() => Some(c.to_string()),
                (Some(p), _) => Some(p.clone()),
                _ => None,
            };
            out.insert(*id, (Some(name.clone()), path.unwrap_or_default()));
        }
        Ok(out)
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

    /// 拉 process id → name 映射。
    async fn fetch_process_names(
        conn: &mut PgConnection,
        ids: &[i64],
    ) -> Result<HashMap<i64, String>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query(
            "SELECT id, name FROM t_process \
             WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
        )
        .bind(ids)
        .fetch_all(conn)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<i64, _>("id"), r.get::<String, _>("name")))
            .collect())
    }

    /// 拉 batch_id → 最近一次 PICKED_UP 事件时间戳。
    async fn fetch_picked_up_at_map(
        conn: &mut PgConnection,
        batch_ids: &[i64],
    ) -> Result<HashMap<i64, NaiveDateTime>, sqlx::Error> {
        if batch_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query(
            "SELECT DISTINCT ON (batch_id) batch_id, created_at \
             FROM t_part_event \
             WHERE batch_id = ANY($1::bigint[]) AND event_type = 'PICKED_UP' \
             ORDER BY batch_id ASC, created_at DESC",
        )
        .bind(batch_ids)
        .fetch_all(conn)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<i64, _>("batch_id"),
                    r.get::<NaiveDateTime, _>("created_at"),
                )
            })
            .collect())
    }
}