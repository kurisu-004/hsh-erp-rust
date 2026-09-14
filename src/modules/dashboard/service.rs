//! dashboard 域业务聚合（2026-09-15 takeover-fill）
//!
//! 对应 Python myERP/service/dashboard.py 的 `build_snapshot_with_workers`：
//! 大屏实时推送的完整快照，包含生产货架分组 + 品检区扁平 + 工人持有件 +
//! 未来 7 天交付分桶。
//!
//! ## 设计要点
//! - 全走运行时 `sqlx::query_as` / `sqlx::query`（与 statistics 域同 pattern，
//!   不依赖 query! 宏离线缓存）
//! - 多个 `Vec<i64>` 收集后批量 `WHERE id IN (...)` 取 name（避免 N+1）
//! - 借 `t_part_batch` 而非 `t_part` 视图（批次级持仓，与 Python 对齐）
//! - `top_n` 默认 1000：远高于合理在持量，仅作防爆兜底

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;
use sqlx::{PgConnection, Row};
use std::collections::HashMap;

const DASHBOARD_TOP_N: i64 = 1000;

/// 大屏快照结构（与 v1 Python 端 JSON 字段命名一致；前端可平滑切 v2 WS）。
#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub on_production_shelves: Vec<OnProductionShelfGroup>,
    pub on_inspection_shelves: Vec<DashboardItem>,
    pub in_process: Vec<DashboardItem>,
    pub upcoming_delivery: Vec<UpcomingDeliveryBucket>,
    pub ts: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnProductionShelfGroup {
    pub shelf_id: String,
    pub shelf_code: String,
    pub shelf_name: String,
    pub total_count: usize,
    pub items: Vec<DashboardItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardItem {
    pub id: String,
    pub batch_id: Option<String>,
    pub batch_no: Option<i32>,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub planned_delivery_date: Option<String>,
    pub picked_up_at: Option<String>,
    pub current_holder_id: Option<String>,
    pub current_holder_kind: Option<String>,
    pub shelf_code: Option<String>,
    pub placed_at: Option<String>,
    pub customer_id: Option<String>,
    pub customer_name: Option<String>,
    pub customer_path: Option<String>,
    pub next_process_id: Option<String>,
    pub next_process_name: Option<String>,
    pub worker_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpcomingDeliveryBucket {
    pub date: String,
    pub count: i64,
}

pub struct DashboardService;

impl DashboardService {
    /// 异步构建一次完整快照。
    ///
    /// 返回 `{on_production_shelves, on_inspection_shelves, in_process,
    ///         upcoming_delivery, ts}`，JSON shape 与 v1 Python 一致。
    pub async fn build_snapshot_with_workers(
        conn: &mut PgConnection,
        top_n: Option<i64>,
    ) -> Result<DashboardSnapshot, sqlx::Error> {
        let top_n = top_n.unwrap_or(DASHBOARD_TOP_N);

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
                "SELECT b.id            AS batch_id, \
                        b.part_id       AS part_id, \
                        b.batch_no      AS batch_no, \
                        b.quantity      AS quantity, \
                        b.current_holder_id AS holder_id, \
                        b.next_process_id   AS next_process_id, \
                        b.placed_at    AS placed_at, \
                        p.id           AS p_id, \
                        p.serial_no    AS serial_no, \
                        p.name         AS p_name, \
                        p.drawing_no   AS drawing_no, \
                        p.is_urgent    AS is_urgent, \
                        p.planned_delivery_date AS planned_delivery_date, \
                        p.customer_id  AS customer_id \
                 FROM t_part_batch b \
                 JOIN t_part p ON p.id = b.part_id \
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

        // 3) 品检区扁平
        let on_insp_rows = fetch_zone_rows(
            &mut *conn,
            "INSPECTION",
            Some("INSPECTION"),
            top_n,
        )
        .await?;

        // 4) 工人持有
        let worker_rows = fetch_worker_rows(&mut *conn, top_n).await?;

        // 5) 批取 name
        let cust_ids: Vec<i64> = on_prod_rows
            .iter()
            .chain(on_insp_rows.iter())
            .chain(worker_rows.iter())
            .map(|(_, p)| p.customer_id)
            .filter(|id| *id > 0)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let cust_map = fetch_customer_path(&mut *conn, &cust_ids).await?;

        let worker_ids: Vec<i64> = worker_rows
            .iter()
            .filter_map(|(b, _)| b.holder_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let worker_name_map = fetch_worker_names(&mut *conn, &worker_ids).await?;

        let mut process_ids: Vec<i64> = Vec::new();
        for (b, _) in on_prod_rows
            .iter()
            .chain(on_insp_rows.iter())
            .chain(worker_rows.iter())
        {
            if let Some(np) = b.next_process_id {
                process_ids.push(np);
            }
        }
        process_ids.sort();
        process_ids.dedup();
        let process_name_map = fetch_process_names(&mut *conn, &process_ids).await?;

        // 6) 分桶：生产区按 current_holder_id，每架取前 10
        let mut rows_by_shelf: HashMap<i64, Vec<(BatchLite, PartLite)>> = HashMap::new();
        for (b, p) in on_prod_rows {
            if let Some(sid) = b.holder_id {
                rows_by_shelf.entry(sid).or_default().push((b, p));
            }
        }
        let prod_groups: Vec<OnProductionShelfGroup> = prod_shelves
            .iter()
            .map(|r| {
                let shelf_id: i64 = r.get("id");
                let shelf_code: String = r.get("code");
                let shelf_name: String = r.get("name");
                let shelf_rows = rows_by_shelf.get(&shelf_id).cloned().unwrap_or_default();
                let total_count = shelf_rows.len();
                let items: Vec<DashboardItem> = shelf_rows
                    .into_iter()
                    .take(10)
                    .map(|(b, p)| {
                        let mut item = part_to_item(&p, &b, &cust_map, &process_name_map);
                        item.shelf_code = Some(shelf_code.clone());
                        item.current_holder_kind = Some("shelf".into());
                        item
                    })
                    .collect();
                OnProductionShelfGroup {
                    shelf_id: shelf_id.to_string(),
                    shelf_code,
                    shelf_name,
                    total_count,
                    items,
                }
            })
            .collect();

        // 7) 品检区扁平
        let insp_items: Vec<DashboardItem> = on_insp_rows
            .into_iter()
            .map(|(b, p)| {
                let mut item = part_to_item(&p, &b, &cust_map, &process_name_map);
                item.current_holder_kind = Some("shelf".into());
                item
            })
            .collect();

        // 8) 工人持有
        let worker_batch_ids: Vec<i64> =
            worker_rows.iter().map(|(b, _)| b.batch_id).collect();
        let picked_at_map = fetch_picked_up_at_map(&mut *conn, &worker_batch_ids).await?;
        let worker_items: Vec<DashboardItem> = worker_rows
            .into_iter()
            .map(|(b, p)| {
                let mut item = part_to_item(&p, &b, &cust_map, &process_name_map);
                item.current_holder_kind = Some("worker".into());
                item.worker_name = b.holder_id.and_then(|wid| worker_name_map.get(&wid).cloned());
                if let Some(bid) = picked_at_map.get(&b.batch_id) {
                    item.picked_up_at = Some(bid.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
                }
                item
            })
            .collect();

        // 9) 未来 7 天交付分桶
        let upcoming = fetch_upcoming_delivery(&mut *conn, 7).await?;
        let ts = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
            .to_string();

        Ok(DashboardSnapshot {
            on_production_shelves: prod_groups,
            on_inspection_shelves: insp_items,
            in_process: worker_items,
            upcoming_delivery: upcoming,
            ts,
        })
    }
}

// ============================================================
// 内部辅助类型与函数（保持单文件职责）
// ============================================================

#[derive(Debug, Clone)]
struct BatchLite {
    batch_id: i64,
    holder_id: Option<i64>,
    next_process_id: Option<i64>,
    placed_at: Option<NaiveDateTime>,
}

#[derive(Debug, Clone)]
struct PartLite {
    part_id: i64,
    serial_no: Option<String>,
    name: String,
    drawing_no: String,
    quantity: i32,
    is_urgent: bool,
    planned_delivery_date: Option<NaiveDate>,
    customer_id: i64,
}

fn row_to_part_batch_pair(r: sqlx::postgres::PgRow) -> (BatchLite, PartLite) {
    (
        BatchLite {
            batch_id: r.get::<i64, _>("batch_id"),
            holder_id: r.try_get::<Option<i64>, _>("holder_id").ok().flatten(),
            next_process_id: r.try_get::<Option<i64>, _>("next_process_id").ok().flatten(),
            placed_at: r.try_get::<Option<NaiveDateTime>, _>("placed_at").ok().flatten(),
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

/// 拉指定 zone 货架上的 IN_PROCESS（生产）或 INSPECTION（品检）批次。
async fn fetch_zone_rows(
    conn: &mut PgConnection,
    zone: &str,
    status: Option<&str>,
    _top_n: i64,
) -> Result<Vec<(BatchLite, PartLite)>, sqlx::Error> {
    let status_value = status.unwrap_or("IN_PROCESS");
    let rows = sqlx::query(
        "WITH active_shelves AS ( \
            SELECT id FROM t_shelf \
            WHERE zone = $1 AND deleted_at IS NULL AND is_active = TRUE \
         ) \
         SELECT b.id            AS batch_id, \
                b.part_id       AS part_id, \
                b.batch_no      AS batch_no, \
                b.quantity      AS quantity, \
                b.current_holder_id AS holder_id, \
                b.next_process_id   AS next_process_id, \
                b.placed_at    AS placed_at, \
                p.id           AS p_id, \
                p.serial_no    AS serial_no, \
                p.name         AS p_name, \
                p.drawing_no   AS drawing_no, \
                p.is_urgent    AS is_urgent, \
                p.planned_delivery_date AS planned_delivery_date, \
                p.customer_id  AS customer_id \
         FROM t_part_batch b \
         JOIN t_part p ON p.id = b.part_id \
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
        "SELECT b.id            AS batch_id, \
                b.part_id       AS part_id, \
                b.batch_no      AS batch_no, \
                b.quantity      AS quantity, \
                b.current_holder_id AS holder_id, \
                b.next_process_id   AS next_process_id, \
                b.placed_at    AS placed_at, \
                p.id           AS p_id, \
                p.serial_no    AS serial_no, \
                p.name         AS p_name, \
                p.drawing_no   AS drawing_no, \
                p.is_urgent    AS is_urgent, \
                p.planned_delivery_date AS planned_delivery_date, \
                p.customer_id  AS customer_id \
         FROM t_part_batch b \
         JOIN t_part p ON p.id = b.part_id \
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

async fn fetch_customer_path(
    conn: &mut PgConnection,
    cust_ids: &[i64],
) -> Result<HashMap<i64, (Option<String>, String)>, sqlx::Error> {
    if cust_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let customers = sqlx::query(
        "SELECT id, name, parent_id FROM t_customer WHERE id = ANY($1::bigint[])",
    )
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
        let rows = sqlx::query(
            "SELECT id, name FROM t_customer WHERE id = ANY($1::bigint[])",
        )
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
        .map(|r| (r.get::<i64, _>("batch_id"), r.get::<NaiveDateTime, _>("created_at")))
        .collect())
}

async fn fetch_upcoming_delivery(
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
    .fetch_all(conn)
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

#[allow(clippy::too_many_arguments)]
fn part_to_item(
    p: &PartLite,
    b: &BatchLite,
    cust_map: &HashMap<i64, (Option<String>, String)>,
    process_map: &HashMap<i64, String>,
) -> DashboardItem {
    let (cust_name, cust_path) = cust_map
        .get(&p.customer_id)
        .cloned()
        .unwrap_or((None, String::new()));
    let np_name = b
        .next_process_id
        .and_then(|np| process_map.get(&np).cloned());
    DashboardItem {
        id: p.part_id.to_string(),
        batch_id: Some(b.batch_id.to_string()),
        batch_no: None,
        serial_no: p.serial_no.clone(),
        name: p.name.clone(),
        drawing_no: p.drawing_no.clone(),
        quantity: p.quantity,
        is_urgent: p.is_urgent,
        planned_delivery_date: p.planned_delivery_date.map(|d| d.format("%Y-%m-%d").to_string()),
        picked_up_at: None,
        current_holder_id: b.holder_id.map(|h| h.to_string()),
        current_holder_kind: None,
        shelf_code: None,
        placed_at: b.placed_at.map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
        customer_id: Some(p.customer_id.to_string()),
        customer_name: cust_name,
        customer_path: Some(cust_path),
        next_process_id: b.next_process_id.map(|np| np.to_string()),
        next_process_name: np_name,
        worker_name: None,
    }
}