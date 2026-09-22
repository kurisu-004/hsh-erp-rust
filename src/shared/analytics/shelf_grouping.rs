//! 按 part 持有者（货架 / 工人）把 `(batch, part)` pairs 分桶（2026-09-22 PR3 重构）
//!
//! 出处：`dashboard/service/snapshot.rs::build_snapshot_with_workers`
//! "按 current_holder_id 分桶限流"段（2026-09-22 Group E 重构后）。
//!
//! ## 行为
//! 遍历 pairs，对每条 `(b, p)` 调用 `key_fn(&b)`：
//! - `Some(k)` → 加入 `out[k]`
//! - `None`    → 跳过（与原内联实现一致；snapshot.rs 中 `holder_id IS NULL` 的批次被丢弃）
//!
//! ## 泛型签名
//! 函数签名接受泛型 `B` / `P` + `key_fn: Fn(&B) -> Option<i64>`，
//! 屏蔽 dashboard 域的 `BatchLite` / `PartLite` 具体类型，便于跨域复用
//! （未来 worker 持有件按 holder_id 分桶也可走本函数）。
//!
//! ## 零 IO
//! 不触 DB、不触 HTTP、不传 `&mut PgConnection`——纯内存分组。

use std::collections::HashMap;

/// 把 `pairs` 按 `key_fn(&b)` 返回的 `Some(i64)` 分桶；`None` 元素被丢弃。
///
/// # Example
/// ```
/// use crate::shared::analytics::shelf_grouping::group_by_shelf;
/// let pairs = vec![(1_i64, "a"), (2, "b"), (1, "c")];
/// let out = group_by_shelf(pairs, |id| Some(*id));
/// assert_eq!(out.get(&1).unwrap().len(), 2);
/// assert_eq!(out.get(&2).unwrap().len(), 1);
/// ```
pub fn group_by_shelf<B, P, F>(pairs: Vec<(B, P)>, key_fn: F) -> HashMap<i64, Vec<(B, P)>>
where
    F: Fn(&B) -> Option<i64>,
{
    let mut out: HashMap<i64, Vec<(B, P)>> = HashMap::new();
    for (b, p) in pairs {
        if let Some(k) = key_fn(&b) {
            out.entry(k).or_default().push((b, p));
        }
    }
    out
}
