//! statistics 域业务逻辑（2026-09-15 takeover-fill）
//!
//! 对应 Python myERP/service/statistics.py。三段：overview / worker_stats /
//! worker_detail + 跳序取件两段（summary / detail）。
//!
//! 约定：
//! - 日期统一转 `[date_from, date_to+1)` 半开区间做 `created_at` 比较；
//! - 贡献度公式与零填充日计数抽离到 `shared::analytics`（2026-09-22 PR3）；
//! - statistics 端点**只读**，不写 DB，不广播 dashboard。
//! - 错误码统一 `BIZ_INVALID_VALUE` / `BIZ_INVALID_QUERY`（与 plan §4.1 决议一致）。
//! - 权限：handler `require_role(Role::Manager)`。

use std::collections::HashMap;

use chrono::NaiveDate;
use sqlx::PgConnection;

use crate::infra::clock::now_naive;
use crate::modules::prod::work_type::repo::WorkTypeRepo;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::modules::statistics::dto::{
    DeliveryPerformance, OverviewOut, PickupSkipDetailItem, PickupSkipDetailOut,
    PickupSkipSummaryItem, PickupSkipSummaryOut, StatusCount, WorkerBrief, WorkerDetailOut,
    WorkerPartItem, WorkerStatsItem, WorkerStatsListOut,
};
use crate::modules::statistics::repo::{
    PickupSkipDetailRow, PickupSkipSummaryRow, StatisticsRepo, WorkerPartRow, WorkerPickupRow,
};
use crate::shared::analytics::{
    daily_buckets::fill_zero_daily_counts, worker_contribution::compute_worker_contribution,
};
use crate::shared::error::{AppError, code};

pub struct StatisticsService;

impl StatisticsService {
    /// 校验日期范围：`date_from > date_to` 抛 400。
    fn validate_date_range(date_from: NaiveDate, date_to: NaiveDate) -> Result<(), AppError> {
        if date_from > date_to {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("date_from ({date_from}) must be <= date_to ({date_to})"),
            ));
        }
        Ok(())
    }

    /// 校验 worker_id 字符串 → i64（雪花 ID）；非法 → 400。
    // 2026-09-15 followup-cleanup A11：错误信息统一为"正整数雪花 ID 字符串"。
    fn parse_worker_id(s: &str) -> Result<i64, AppError> {
        s.parse::<i64>().map_err(|_| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                "worker_id 必须是正整数雪花 ID 字符串",
            )
        })
    }

    // ============================================================
    // tab1: Overview
    // ============================================================

    pub async fn overview(
        conn: &mut PgConnection,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<OverviewOut, AppError> {
        Self::validate_date_range(date_from, date_to)?;
        let today = now_naive().date();

        let created_count = StatisticsRepo::count_created(conn, date_from, date_to).await?;
        let completed_count = StatisticsRepo::count_completed(conn, date_from, date_to).await?;
        let in_process_count = StatisticsRepo::count_in_process_at(conn, date_to).await?;
        let (delivered_count, delivered_value, orange, red) =
            StatisticsRepo::delivered_stats(conn, date_from, date_to).await?;
        let overdue_undelivered = StatisticsRepo::count_overdue_undelivered(conn, today).await?;
        let repair_count = StatisticsRepo::count_repair_parts(conn, date_from, date_to).await?;
        let daily_created_raw =
            StatisticsRepo::daily_created_counts(conn, date_from, date_to).await?;
        let daily_completed_raw =
            StatisticsRepo::daily_completed_counts(conn, date_from, date_to).await?;
        let status_dist = StatisticsRepo::status_distribution(conn).await?;

        let daily_created = fill_zero_daily_counts(date_from, date_to, &daily_created_raw);
        let daily_completed = fill_zero_daily_counts(date_from, date_to, &daily_completed_raw);

        // on_time = delivered - orange - red（互斥拆分）
        let on_time = (delivered_count - orange - red).max(0);

        Ok(OverviewOut {
            date_from,
            date_to,
            created_count,
            completed_count,
            in_process_count,
            delivered_count,
            delivered_value,
            late_orange_count: orange,
            late_red_count: red,
            overdue_undelivered_count: overdue_undelivered,
            repair_part_count: repair_count,
            daily_created,
            daily_completed,
            delivery_performance: DeliveryPerformance {
                on_time,
                orange,
                red,
            },
            status_distribution: status_dist
                .into_iter()
                .map(|(status_value, count)| StatusCount {
                    status_value,
                    count,
                })
                .collect(),
        })
    }

    // ============================================================
    // tab2: WorkerStats
    // ============================================================

    pub async fn worker_stats(
        conn: &mut PgConnection,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<WorkerStatsListOut, AppError> {
        Self::validate_date_range(date_from, date_to)?;

        // 全部未软删工人（不分页 — 内部工人规模远小于 500）。
        // 2026-09-15 followup-cleanup A9：明确 1000 远高于合理在持工人数（防爆兜底；如需全量应走分页）
        let workers_rows = WorkerRepo::list_with_filters(&mut *conn, None, None, 1000, 0).await?;

        let work_type_ids: Vec<i64> = workers_rows
            .iter()
            .filter_map(|w| w.work_type_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let wt_rows = WorkTypeRepo::list_by_ids(&mut *conn, &work_type_ids).await?;
        let wt_name_map: HashMap<i64, String> =
            wt_rows.into_iter().map(|wt| (wt.id, wt.name)).collect();

        let rows = StatisticsRepo::worker_pickup_rows(&mut *conn, date_from, date_to).await?;
        let agg_map: HashMap<i64, WorkerPickupRow> =
            rows.iter().map(|r| (r.worker_id, r.clone())).collect();

        let contribution_map = compute_worker_contribution(&rows);

        let items = workers_rows
            .into_iter()
            .map(|w| {
                let agg = agg_map.get(&w.id);
                let wt_name = w.work_type_id.and_then(|id| wt_name_map.get(&id).cloned());
                WorkerStatsItem {
                    worker_id: w.id,
                    worker_name: w.name,
                    badge_code: w.badge_code,
                    work_type_id: w.work_type_id,
                    work_type_name: wt_name,
                    is_active: w.is_active,
                    pickup_count: agg.map(|a| a.pickup_count).unwrap_or(0),
                    pickup_quantity: agg.map(|a| a.pickup_quantity).unwrap_or(0),
                    participated_part_count: agg.map(|a| a.participated_part_count).unwrap_or(0),
                    contribution_pct: contribution_map.get(&w.id).copied().flatten(),
                }
            })
            .collect();

        Ok(WorkerStatsListOut { items })
    }

    // ============================================================
    // tab3: WorkerDetail
    // ============================================================

    pub async fn worker_detail(
        conn: &mut PgConnection,
        worker_id_str: &str,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<WorkerDetailOut, AppError> {
        Self::validate_date_range(date_from, date_to)?;
        let wid_int = Self::parse_worker_id(worker_id_str)?;

        let worker = WorkerRepo::get_by_id(&mut *conn, wid_int, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORKER_NOT_FOUND,
                    format!("worker {worker_id_str} 不存在或已软删"),
                )
            })?;

        let work_type_name: Option<String> = if let Some(wt_id) = worker.work_type_id {
            WorkTypeRepo::get_by_id(&mut *conn, wt_id)
                .await?
                .map(|wt| wt.name)
        } else {
            None
        };

        let (pickup_count, pickup_quantity, return_count) =
            StatisticsRepo::worker_detail_events(&mut *conn, wid_int, date_from, date_to).await?;
        let daily_pickups_raw =
            StatisticsRepo::worker_daily_pickups(&mut *conn, wid_int, date_from, date_to).await?;
        let parts_rows =
            StatisticsRepo::worker_parts(&mut *conn, wid_int, date_from, date_to).await?;
        let daily_pickups = fill_zero_daily_counts(date_from, date_to, &daily_pickups_raw);

        // distinct part_id（即便 part 已软删也计数 — 与 parts 列表口径不同）
        let participated_part_count = parts_rows
            .iter()
            .map(|r| r.part_id)
            .collect::<std::collections::HashSet<_>>()
            .len() as i64;

        let items = parts_rows
            .into_iter()
            .map(|r: WorkerPartRow| WorkerPartItem {
                part_id: r.part_id,
                serial_no: r.serial_no,
                name: r.name,
                drawing_no: r.drawing_no,
                status: r.status,
                pickup_count: r.pickup_count,
                last_pickup_at: r.last_pickup_at,
            })
            .collect();

        Ok(WorkerDetailOut {
            worker: WorkerBrief {
                id: worker.id,
                name: worker.name,
                badge_code: worker.badge_code,
                work_type_name,
                is_active: worker.is_active,
            },
            pickup_count,
            pickup_quantity,
            participated_part_count,
            return_count,
            daily_pickups,
            parts: items,
        })
    }

    // ============================================================
    // tab4 跳序取件
    // ============================================================

    pub async fn pickup_skip_summary(
        conn: &mut PgConnection,
    ) -> Result<PickupSkipSummaryOut, AppError> {
        let rows = StatisticsRepo::pickup_skip_summary(conn).await?;
        let items = rows
            .into_iter()
            .map(|r: PickupSkipSummaryRow| PickupSkipSummaryItem {
                worker_id: r.worker_id,
                worker_name: r.worker_name,
                badge_code: r.badge_code,
                work_type_name: r.work_type_name,
                skip_count: r.skip_count,
                last_skip_at: r.last_skip_at,
            })
            .collect();
        Ok(PickupSkipSummaryOut { items })
    }

    pub async fn pickup_skip_detail(
        conn: &mut PgConnection,
        worker_id_str: &str,
        limit: i64,
        offset: i64,
    ) -> Result<PickupSkipDetailOut, AppError> {
        let wid_int = Self::parse_worker_id(worker_id_str)?;
        let limit = limit.clamp(1, 200);
        let offset = offset.max(0);

        let rows = StatisticsRepo::pickup_skip_detail(conn, wid_int, limit, offset).await?;
        let total = StatisticsRepo::pickup_skip_detail_count(conn, wid_int).await?;

        let items = rows
            .into_iter()
            .map(|r: PickupSkipDetailRow| PickupSkipDetailItem {
                id: r.id,
                part_id: r.part_id,
                serial_no: r.serial_no,
                part_name: r.part_name,
                batch_no: r.batch_no,
                quantity: r.quantity,
                part_planned_delivery_date: r.part_planned_delivery_date,
                skipped_earliest_date: r.skipped_earliest_date,
                created_at: r.created_at,
            })
            .collect();

        Ok(PickupSkipDetailOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

// 2026-09-22 PR3：原 `compute_contribution` / `zero_fill_day_count` 两个私有 fn 已抽离到
// `crate::shared::analytics::{worker_contribution, daily_buckets}`，行为 1:1 保留。
// 单元测试随抽离搬到各 fn 所在文件，service.rs 此处不再重复。

// ============================================================
// 2026-09-15 followup-cleanup A7：删除原 `_unused()` 死代码（Role / Decimal 实际由上层 handler 引用）
// ============================================================
