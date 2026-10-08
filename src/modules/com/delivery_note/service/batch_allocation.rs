//! DP 批次分配：把「入单件数」分配到「该零件可入单的批次」上。
//!
//! ## 两条业务原则（原文）
//! 1. **如无必要不拆批** —— 优先选零拆批方案；
//! 2. **不与 1 冲突时优先入单小批** —— 零拆批方案有多个时取「升序序列字典序最小」的。
//!
//! 例（target = 10 件）：
//! - 候选 `{2, 3, 9, 10}`：零拆批解唯一 ⇒ `[(10, 10)]`。
//! - 候选 `{2, 3, 5, 10}`：零拆批解有 `{10}` 与 `{2,3,5}` 两个 ⇒ 原则 2 取升序序列
//!   字典序更小的 `{2, 3, 5}` ⇒ `[(2,2), (3,3), (5,5)]`。
//!
//! ## 算法
//! ```text
//! 1. 全体子集和 DP：后向可达表 can[k][q] = 「只用下标 ≥ k 的候选能否凑出 q」
//! 2. can[0][target] ⇒ 升序逐位最小化回溯（拆批数 0）
//! 3. 否则：排除 quantity 最大的那个批次再跑一次 DP 得 max_reachable
//!    差额 = target - max_reachable，从被排除的最大批次拆出差额（拆批数 1）
//!    差额 <= 0（= 不可达，说明 target > Σ 全部数量）⇒ Err(21405)
//! 4. n > 100 || target > 100_000 ⇒ 降级贪心（见下）
//! ```
//!
//! ### 回溯方向：为什么「从小到大、取最小候选且余量仍可达」
//! 解是候选的一个子集，它的**升序数量序列**就是解本身。要让该序列字典序最小，
//! 就得让**首个元素尽可能小**、再让次个元素尽可能小……即「逐位最小化」。这正是上面
//! 第 2 步的写法：候选按 `quantity ASC, batch_no ASC` 升序排列，从下标 0 往后扫，
//! 能取就取（取的前提是 `can[k+1][remaining - q_k]` 为真，即**余量仍可被后面的候选
//! 凑出**）。
//!
//! 「能取就取」若不加余量可达前提就会退化成「能凑多少凑多少」的贪心，产出的是**任意**
//! 零拆批解而非字典序最小者 —— 例 2 的候选 `{2,3,5,10}` 里无前提地一路取小会卡在
//! `{2,3}` 后余 5 可取 5 得 `{2,3,5}`（本例恰好正确），而候选 `{2,3,9,10}` / target 10
//! 无前提取小会卡在余量不可达处；后向可达表让每一步都有「取得下」的判据，故两个例子
//! 都稳定给出规格要求的解。
//!
//! ## 降级贪心（G1）的阈值与语义
//! `n > 100`（候选批次过多）或 `target > 100_000`（件数异常大）时，`O(n × target)`
//! 的 DP 表会吃掉几百 MB 内存。此时降级为贪心 G1：「按升序逐批取，取满 target 即
//! 停，最后一批取剩余量」。G1 可能产生拆批（原则 1 被牺牲），但不会算不出结果。
//! 该降级在 `docs/api/delivery_note.md` §8.4 有登记。
//!
//! ## 与「装配件整套」的分工
//! 装配件下**每个子件独立跑一次**本函数（套数已由 `entry_max_sets` 的 `min` 定死，
//! 子件之间不耦合）⇒ 没有跨子件的组合爆炸。

use crate::shared::batch::TPartBatch;
use crate::shared::error::{AppError, code};

/// DP 表的候选批次条数上限（超过即降级贪心）。
const DP_MAX_CANDIDATES: usize = 100;
/// DP 表的 target 上限（超过即降级贪心）。
const DP_MAX_TARGET: i32 = 100_000;

/// 把 `target` 件数分配到 `eligible` 批次上。
///
/// 入参 `eligible` 必须是「**可入单**的批次行」：`status == "READY_TO_SHIP"` 且
/// `delivery_note_id IS NULL`（未占用）的活跃批次。函数**不做**这两个闸门。口径由
/// `DeliveryScanRepo::list_entryable_batches_by_part_ids`（`repo/scan_tree.rs`）的 SQL
/// 保证 —— 那是「可入单」的唯一定义；`POST /scan` 的分类循环只按同一口径把非 READY 批次与
/// 被别的送货单占用的批次都收进诊断明细（分配失败时附进 21406 / 21405 的 message），
/// 两者都不参与本函数的候选筛选。
///
/// 返回 `Vec<(batch_id, quantity)>`。
///
/// **DP 路径（正常输入）**按 `batch_id` 升序（`to_allocation` / `split_largest` 末尾
/// 各排一次）；**降级贪心 G1 路径按入参顺序**（`greedy` 不排序，而入参序是
/// `quantity ASC, batch_no ASC`）。两条路径的顺序都不保证是 `batch_id` 升序以外的
/// 任何语义 ⇒ 调用方（`POST /scan` 的挂单循环）不要依赖返回顺序，它按批次逐条处理。
///
/// `quantity < batch.quantity` 的项表示该批次被拆分，调用方需对**差额**新建拆批
/// 并把差额挂单（原批次保持不动、不挂单 —— 见 `POST /scan` 的 Step 7）。
///
/// 失败：`target <= 0`、target 大于候选总量、或降级路径凑不出 → 21405
/// `BIZ_DELIVERY_NOTE_PART_NOT_READY`。
pub(crate) fn allocate(target: i32, eligible: &[TPartBatch]) -> Result<Vec<(i64, i32)>, AppError> {
    if target <= 0 {
        return Err(not_enough(target, 0));
    }
    if eligible.is_empty() {
        return Err(not_enough(target, 0));
    }
    let total: i32 = eligible.iter().map(|b| b.quantity).sum();
    if target > total {
        return Err(not_enough(target, total));
    }

    if eligible.len() > DP_MAX_CANDIDATES || target > DP_MAX_TARGET {
        return Ok(greedy(target, eligible));
    }
    match subset_sum(target, eligible) {
        Some(picked) => Ok(to_allocation(&picked, eligible)),
        // 零拆批不可达：牺牲原则 1，从最大的批次拆出差额。
        None => split_largest(target, eligible, total),
    }
}

/// 全体子集和 DP；返回「升序数量序列字典序最小」的那个解（候选下标集合，升序）。
///
/// 后向可达表 `can[k][q]` = 「只用下标 ≥ k 的候选能否凑出 q」，据此**从小到大**
/// 贪心：候选 `k` 若「取它之后剩余量仍可凑出」就取，否则跳过。
///
/// 为什么这个贪心等价于「字典序最小」：解是候选的一个子集，其升序数量序列就是
/// 解本身。要让该序列字典序最小，就要让**首个元素尽可能小** —— 于是「取最小的那个
/// 候选，只要剩余量还凑得出来」正是逐位最小化的写法。验证两个规格例子：
/// - `{2,3,9,10}` / target 10：取 2 后 8 凑不出 ⇒ 跳过；取 3 后 7 凑不出 ⇒ 跳过；
///   取 9 后 1 凑不出 ⇒ 跳过；取 10 后 0 可凑 ⇒ 得 `{10}`（唯一零拆批解）。
/// - `{2,3,5,10}` / target 10：取 2 后 8 = 3+5 可凑 ⇒ 取；取 3 后 5 可凑 ⇒ 取；
///   取 5 后 0 可凑 ⇒ 取 ⇒ 得 `{2,3,5}`（比 `{10}` 的升序序列字典序更小）。
fn subset_sum(target: i32, eligible: &[TPartBatch]) -> Option<Vec<usize>> {
    let target = target as usize;
    let n = eligible.len();
    // can[k][q]：只用下标 ≥ k 的候选能否凑出 q。can[n][0] = true，其余 false。
    let mut can = vec![vec![false; target + 1]; n + 1];
    can[n][0] = true;
    for k in (0..n).rev() {
        let q_b = eligible[k].quantity as usize;
        for q in 0..=target {
            can[k][q] = can[k + 1][q] || (q >= q_b && can[k + 1][q - q_b]);
        }
    }
    if !can[0][target] {
        return None;
    }
    let mut out = Vec::new();
    let mut remaining = target;
    for k in 0..n {
        let q_b = eligible[k].quantity as usize;
        if remaining >= q_b && can[k + 1][remaining - q_b] {
            out.push(k);
            remaining -= q_b;
        }
    }
    debug_assert_eq!(remaining, 0, "回溯后剩余量必须为 0（DP 表保证）");
    Some(out)
}

/// 零拆批不可达时：排除 quantity 最大的那一个，其余再跑一次 DP，把差额从最大批次
/// 拆出来（拆批数恒为 1）。
///
/// ⚠️ 「最大」按 `quantity DESC, batch_no ASC` 定：数量相同时取 batch_no 小的那个，
/// 保证同一输入的分配结果稳定可复现。
fn split_largest(
    target: i32,
    eligible: &[TPartBatch],
    total: i32,
) -> Result<Vec<(i64, i32)>, AppError> {
    let mut idx: Vec<usize> = (0..eligible.len()).collect();
    idx.sort_by(|&a, &b| {
        eligible[b]
            .quantity
            .cmp(&eligible[a].quantity)
            .then_with(|| eligible[a].batch_no.cmp(&eligible[b].batch_no))
    });
    let drop = idx[0];
    let dropped = &eligible[drop];
    let rest: Vec<TPartBatch> = eligible
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != drop)
        .map(|(_, b)| b.clone())
        .collect();

    // 差额 = target - 「其余候选能凑出的、不超过 target 的最大值」；差额恒为正，
    // 且必须 ≤ 被排除批次的数量（否则这个拆分方案本身不合法 ⇒ fail-loud）。
    let rest_best = match subset_sum(target, &rest) {
        Some(picked) => {
            let sum: i32 = picked.iter().map(|&i| rest[i].quantity).sum();
            debug_assert_eq!(sum, target);
            return Ok(to_allocation(&picked, eligible));
        }
        None => best_reachable(target, &rest),
    };
    let delta = target - rest_best;
    if delta <= 0 || delta > dropped.quantity {
        return Err(not_enough(target, total));
    }
    let picked = subset_sum(rest_best, &rest).ok_or_else(|| not_enough(target, total))?;
    let mut out: Vec<(i64, i32)> = picked
        .iter()
        .map(|&i| (rest[i].id, rest[i].quantity))
        .collect();
    out.push((dropped.id, delta));
    out.sort_by_key(|(id, _)| *id);
    Ok(out)
}

/// `rest` 能凑出的、不超过 `target` 的最大子集和。
fn best_reachable(target: i32, rest: &[TPartBatch]) -> i32 {
    let cap = target as usize;
    let mut reachable = vec![false; cap + 1];
    reachable[0] = true;
    for b in rest {
        let q_b = b.quantity as usize;
        if q_b > cap {
            continue;
        }
        for q in (q_b..=cap).rev() {
            if reachable[q - q_b] {
                reachable[q] = true;
            }
        }
    }
    reachable.iter().rposition(|&v| v).unwrap_or(0) as i32
}

/// 降级贪心 G1：升序逐批取满即停，末批取剩余量。
///
/// ⚠️ 可能拆批（牺牲原则 1），换来 O(n) 的复杂度与恒定内存。见模块 doc「降级贪心」。
fn greedy(target: i32, eligible: &[TPartBatch]) -> Vec<(i64, i32)> {
    let mut remaining = target;
    let mut out = Vec::new();
    for b in eligible {
        if remaining <= 0 {
            break;
        }
        let take = remaining.min(b.quantity);
        if take <= 0 {
            continue;
        }
        out.push((b.id, take));
        remaining -= take;
    }
    out
}

/// DP 解（候选下标）→ `(batch_id, quantity)` 分配表，按 batch_id 升序。
fn to_allocation(picked: &[usize], eligible: &[TPartBatch]) -> Vec<(i64, i32)> {
    let mut out: Vec<(i64, i32)> = picked
        .iter()
        .map(|&i| (eligible[i].id, eligible[i].quantity))
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

/// 「凑不出这么多件」的唯一错误出口。
fn not_enough(target: i32, total: i32) -> AppError {
    AppError::biz(
        code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
        format!("可入单件数不足：需要 {target} 件，候选批次合计 {total} 件"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn now() -> NaiveDateTime {
        crate::infra::clock::now_naive()
    }

    /// 造一个可入单批次（`status = READY_TO_SHIP`、未占用）。
    fn b(id: i64, batch_no: i32, quantity: i32) -> TPartBatch {
        TPartBatch {
            id,
            part_id: 100,
            batch_no,
            quantity,
            status: "READY_TO_SHIP".to_string(),
            location: Some("PRODUCTION_SHELF".to_string()),
            current_holder_id: None,
            current_process_id: None,
            current_process_step_id: None,
            delivery_note_id: None,
            delivery_seq: None,
            parent_batch_id: None,
            is_repairing: false,
            version: 0,
            created_at: now(),
            created_by: None,
            updated_at: now(),
            updated_by: None,
            deleted_at: None,
        }
    }

    /// 把数量序列转成「按数量升序、id = 1..n」的候选批次。
    fn cands(quantities: &[i32]) -> Vec<TPartBatch> {
        let mut v: Vec<TPartBatch> = quantities
            .iter()
            .enumerate()
            .map(|(i, q)| b(i as i64 + 1, i as i32 + 1, *q))
            .collect();
        v.sort_by_key(|x| (x.quantity, x.batch_no));
        v
    }

    /// 规格 §4.3 的例 1：target 10，候选 {2,3,9,10} ⇒ 零拆批解唯一 `{10}`。
    #[test]
    fn unique_zero_split_solution_picks_the_exact_batch() {
        let out = allocate(10, &cands(&[2, 3, 9, 10])).unwrap();
        assert_eq!(out, vec![(4, 10)]);
    }

    /// 规格 §4.3 的例 2：target 10，候选 {2,3,5,10} ⇒ 零拆批解里字典序最小的是
    /// `{2,3,5}` 而不是 `{10}`。
    #[test]
    fn multiple_zero_split_solutions_pick_lexicographically_smallest() {
        let out = allocate(10, &cands(&[2, 3, 5, 10])).unwrap();
        assert_eq!(out, vec![(1, 2), (2, 3), (3, 5)]);
    }

    /// 零拆批不可达 ⇒ 从最大批次拆出差额，且只拆 1 个批次。
    #[test]
    fn unreachable_exact_match_splits_largest_batch_once() {
        // target 7，候选 {3, 4}：3+4=7 可零拆批；改 target 8 ⇒ 超总量，走 not_enough。
        // target 5，候选 {3, 4}：无子集和为 5 ⇒ 排除 4 后 3 可凑，差额 2 从 4 拆。
        let out = allocate(5, &cands(&[3, 4])).unwrap();
        assert_eq!(out, vec![(1, 3), (2, 2)]);
    }

    /// target 大于候选总量 ⇒ 21405（凑不出，不是「部分满足」）。
    #[test]
    fn target_beyond_total_is_not_enough() {
        let err = allocate(11, &cands(&[3, 4])).unwrap_err();
        assert_eq!(err.code(), code::BIZ_DELIVERY_NOTE_PART_NOT_READY);
    }

    /// 目标恰好等于总量 ⇒ 零拆批全取。
    #[test]
    fn target_equal_to_total_takes_everything() {
        let out = allocate(9, &cands(&[2, 3, 4])).unwrap();
        assert_eq!(out, vec![(1, 2), (2, 3), (3, 4)]);
    }

    /// 候选恰好等于目标 ⇒ 直接整批，不拆。
    #[test]
    fn single_exact_batch_needs_no_split() {
        let out = allocate(7, &cands(&[7, 9])).unwrap();
        assert_eq!(out, vec![(1, 7)]);
    }

    /// 空候选 / 非正 target ⇒ 21405。
    #[test]
    fn empty_or_non_positive_target_is_not_enough() {
        assert!(allocate(1, &[]).is_err());
        assert!(allocate(0, &cands(&[3])).is_err());
        assert!(allocate(-1, &cands(&[3])).is_err());
    }

    /// 降级阈值：候选数超 100 ⇒ 走贪心，结果仍凑满 target。
    #[test]
    fn degrade_to_greedy_when_candidates_exceed_threshold() {
        let many: Vec<TPartBatch> = (0..150).map(|i| b(i + 1, i as i32 + 1, 1)).collect();
        let out = allocate(150, &many).unwrap();
        let sum: i32 = out.iter().map(|(_, q)| *q).sum();
        assert_eq!(sum, 150, "贪心路径也必须凑满 target");
        assert_eq!(out.len(), 150);
    }

    /// 降级阈值：target 超 100_000 ⇒ 走贪心。
    #[test]
    fn degrade_to_greedy_when_target_exceeds_threshold() {
        let out = allocate(100_001, &cands(&[60_000, 60_000])).unwrap();
        let sum: i32 = out.iter().map(|(_, q)| *q).sum();
        assert_eq!(sum, 100_001);
    }

    /// **DP 路径**的分配表按 batch_id 升序（前端可逐字比对，不必二次排序）。
    ///
    /// ⚠️ 只对 DP 路径成立：降级贪心 G1 按入参序返回（见 `allocate` 的 doc）。
    /// `cands(&[1,2,3,4,5])` 的入参恰好 id 升序，故本用例走的是 DP 路径。
    #[test]
    fn dp_path_allocation_is_sorted_by_batch_id() {
        let out = allocate(6, &cands(&[1, 2, 3, 4, 5])).unwrap();
        let ids: Vec<i64> = out.iter().map(|(id, _)| *id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }
}
