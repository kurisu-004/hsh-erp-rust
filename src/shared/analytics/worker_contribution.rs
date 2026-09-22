//! 工人贡献度公式（同工种内 pickup_count 占比，2026-09-22 PR3 重构）
//!
//! 出处：`statistics/service.rs::compute_contribution`（2026-09-15 takeover-fill）。
//!
//! ## 公式
//! `worker.pickup_count / 同工种 total * 100`，保留 2 位小数。
//! 无工种或工种总领取为 0 → `None`（前端展示「—」）。
//!
//! ## 零 IO
//! 不触 DB、不触 HTTP，纯内存计算。

use std::collections::HashMap;

use crate::modules::statistics::repo::WorkerPickupRow;

/// 计算工人贡献度：`worker.pickup_count / 同工种 total * 100`，保留 2 位小数。
///
/// 返回 `HashMap<worker_id, Option<f64>>`：无工种或工种总领取为 0 → `None`。
pub fn compute_worker_contribution(rows: &[WorkerPickupRow]) -> HashMap<i64, Option<f64>> {
    // 按 work_type_id 聚合 total
    let mut totals_by_wt: HashMap<i64, i64> = HashMap::new();
    let mut workers_by_wt: HashMap<i64, Vec<&WorkerPickupRow>> = HashMap::new();
    let mut no_work_type: Vec<&WorkerPickupRow> = Vec::new();

    for r in rows {
        if let Some(wt_id) = r.work_type_id {
            *totals_by_wt.entry(wt_id).or_insert(0) += r.pickup_count;
            workers_by_wt.entry(wt_id).or_default().push(r);
        } else {
            no_work_type.push(r);
        }
    }

    let mut out: HashMap<i64, Option<f64>> = HashMap::new();
    for r in no_work_type {
        out.insert(r.worker_id, None);
    }
    for (wt_id, total) in totals_by_wt {
        for r in workers_by_wt.get(&wt_id).cloned().unwrap_or_default() {
            let pct = if total <= 0 {
                None
            } else {
                let p = r.pickup_count as f64 / total as f64 * 100.0;
                Some((p * 100.0).round() / 100.0)
            };
            out.insert(r.worker_id, pct);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_worker_contribution_no_work_type_yields_none() {
        let rows = vec![WorkerPickupRow {
            worker_id: 1,
            work_type_id: None,
            pickup_count: 5,
            pickup_quantity: 0,
            participated_part_count: 0,
        }];
        let m = compute_worker_contribution(&rows);
        assert_eq!(m.get(&1), Some(&None));
    }

    #[test]
    fn compute_worker_contribution_per_work_type() {
        let rows = vec![
            WorkerPickupRow {
                worker_id: 1,
                work_type_id: Some(100),
                pickup_count: 3,
                pickup_quantity: 0,
                participated_part_count: 0,
            },
            WorkerPickupRow {
                worker_id: 2,
                work_type_id: Some(100),
                pickup_count: 7,
                pickup_quantity: 0,
                participated_part_count: 0,
            },
        ];
        let m = compute_worker_contribution(&rows);
        assert_eq!(m.get(&1).and_then(|x| *x), Some(30.0));
        assert_eq!(m.get(&2).and_then(|x| *x), Some(70.0));
    }
}
