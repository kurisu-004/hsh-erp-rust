//! 把 `[(NaiveDate, i64)]` 按 `[date_from, date_to]` 闭区间补齐为 0（2026-09-22 PR3 重构）
//!
//! 出处：`statistics/service.rs::zero_fill_day_count`（2026-09-15 takeover-fill）。
//!
//! ## 行为
//! 遍历 `[date_from, date_to]` 闭区间每日：原 `raw` 中有的 → 用原值；没有 → 计数为 0。
//! 区间过长（`date_to.succ_opt() == None`）→ panic（与原内联实现一致；statistics 端点
//! 日期范围由 service 校验，不会触发）。
//!
//! ## 零 IO
//! 不触 DB、不触 HTTP，纯内存日期遍历。

use chrono::NaiveDate;
use std::collections::HashMap;

use crate::modules::statistics::vo::DayCount;

/// 把 `raw: &[(NaiveDate, i64)]` 按 `[date_from, date_to]` 闭区间补齐为 0。
pub fn fill_zero_daily_counts(
    date_from: NaiveDate,
    date_to: NaiveDate,
    raw: &[(NaiveDate, i64)],
) -> Vec<DayCount> {
    let map: HashMap<NaiveDate, i64> = raw.iter().cloned().collect();
    let mut out: Vec<DayCount> = Vec::new();
    let mut cur = date_from;
    while cur <= date_to {
        out.push(DayCount {
            date: cur,
            count: *map.get(&cur).unwrap_or(&0),
        });
        cur = cur
            .succ_opt()
            .expect("date overflow in fill_zero_daily_counts");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_fill_missing_dates() {
        let from = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 9, 3).unwrap();
        let raw = vec![(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(), 5i64)];
        let out = fill_zero_daily_counts(from, to, &raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].count, 5);
        assert_eq!(out[1].count, 0);
        assert_eq!(out[2].count, 0);
    }

    #[test]
    fn zero_fill_full_range() {
        let from = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 9, 2).unwrap();
        let raw = vec![
            (NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(), 1),
            (NaiveDate::from_ymd_opt(2026, 9, 2).unwrap(), 2),
        ];
        let out = fill_zero_daily_counts(from, to, &raw);
        assert_eq!(out[0].count, 1);
        assert_eq!(out[1].count, 2);
    }
}
