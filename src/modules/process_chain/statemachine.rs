//! process_chain 域纯函数（无 DB、无 IO）
//!
//! 当前阶段只放 sort_order 间隙检测 / 重排 helper；将来如有 sort 精度耗尽（差值=1）
//! 触发 `reorder_with_step_size_10` 批量重排的需求，本模块继续扩展。
//!
//! ## 约定
//! - 全部为 `pub fn` + 单元测试覆盖（conventions.md §4.1：纯函数必须 100% 行覆盖）
//! - 不引入 IO/全局可变状态依赖
//!
//! ## ⚠️ 2026-09-17 PR-4 卫生项 B5
//! - `gap_for_mid_insert` / `reorder_with_step_size` 当前无业务调用方（PR-1
//!   upsert_chain 走「先删旧 steps + 整组 bulk_insert」模式，不触发"中间
//!   插入"路径）；保留为将来 sort 精度耗尽时「批量重排」入口。
//! - 单元测试仍 100% 行覆盖（conventions.md §4.1 强制）；不删 fn 也不加
//!   `#[allow(dead_code)]` 抑制，让 rustc 持续提示直到实际接入。

/// 检测相邻步骤 sort_order 间隙是否还够"中间插入"。
///
/// 稀疏 sort 策略：`10/20/30`（step_size=10）。当 `(prev + next) / 2` 仍是整数，
/// 且与 prev / next 都不同 ⇒ 可以中间插入；否则返回 `false` 表示需触发批量重排。
///
/// 输入要求：
/// - `prev` < `next`（已排序的不相邻 sort_order 值；调用方负责升序）
///
/// 边界：
/// - 相邻差值 = 2（10/12）⇒ `(10+12)/2 = 11` ⇒ `Ok(11)`
/// - 相邻差值 = 1（10/11）⇒ `(10+11)/2 = 10` ⇒ `NoGap`（精度耗尽）
/// - 相邻差值 > 1 但奇偶冲突（10/13）⇒ `(10+13)/2 = 11` ⇒ `Ok(11)`
#[allow(dead_code)] // ⚠️ 2026-09-17 PR-4 B5：当前无业务调用方，保留作 sort 精度耗尽场景入口
pub fn gap_for_mid_insert(prev: i32, next: i32) -> Option<i32> {
    if prev < 0 || next < 0 {
        return None;
    }
    if prev >= next {
        return None;
    }
    // 中点
    let mid = (prev as i64 + next as i64) / 2;
    if mid <= prev as i64 || mid >= next as i64 {
        return None;
    }
    Some(mid as i32)
}

/// 把已有步骤序列重新规整为 `step_size` 等间距（默认 10）。
///
/// 输入：`items` 是按 sort_order 升序的已有 sort_order 列表；
/// 返回：新 sort_order 序列，与 `items` 等长，从 `step_size` 开始累加。
///
/// 触发场景（业务）：
/// - UI 拖拽中间插入到 (prev, next) 差值 = 1 时，service 层无法"中间插入"，
///   由前端触发 `reorder_steps` 把整链重排成 10/20/30 后再插入。
///
/// 单测覆盖（conventions.md §4.1）：
/// - 空列表 → 空 Vec
/// - 1 项 → [10]
/// - 3 项 → [10, 20, 30]
/// - step_size=5 → [10, 15, 20]
#[allow(dead_code)] // ⚠️ 2026-09-17 PR-4 B5：当前无业务调用方，保留作 sort 精度耗尽场景入口
pub fn reorder_with_step_size(sort_orders: &[i32], step_size: i32) -> Vec<i32> {
    if step_size <= 0 {
        // 防御：step_size 非正 → 退化到 10
        return reorder_with_step_size(sort_orders, 10);
    }
    sort_orders
        .iter()
        .enumerate()
        .map(|(i, _)| (i as i32 + 1) * step_size)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_for_mid_insert_even_gap_returns_mid() {
        assert_eq!(gap_for_mid_insert(10, 20), Some(15));
        assert_eq!(gap_for_mid_insert(10, 30), Some(20));
        assert_eq!(gap_for_mid_insert(20, 30), Some(25));
    }

    #[test]
    fn gap_for_mid_insert_odd_gap_returns_floor() {
        // (10 + 13) / 2 = 11 (整数除法 floor)
        assert_eq!(gap_for_mid_insert(10, 13), Some(11));
        // (10 + 12) / 2 = 11
        assert_eq!(gap_for_mid_insert(10, 12), Some(11));
    }

    #[test]
    fn gap_for_mid_insert_returns_none_on_unit_gap() {
        // 差值=1 → 精度耗尽
        assert_eq!(gap_for_mid_insert(10, 11), None);
    }

    #[test]
    fn gap_for_mid_insert_returns_none_on_invalid_range() {
        assert_eq!(gap_for_mid_insert(10, 10), None);
        assert_eq!(gap_for_mid_insert(20, 10), None);
        assert_eq!(gap_for_mid_insert(-1, 10), None);
        assert_eq!(gap_for_mid_insert(10, -1), None);
    }

    #[test]
    fn reorder_with_step_size_empty() {
        let v: Vec<i32> = vec![];
        assert_eq!(reorder_with_step_size(&v, 10), Vec::<i32>::new());
    }

    #[test]
    fn reorder_with_step_size_default() {
        let v = vec![0, 5, 7];
        assert_eq!(reorder_with_step_size(&v, 10), vec![10, 20, 30]);
    }

    #[test]
    fn reorder_with_step_size_custom() {
        let v = vec![1, 2, 3];
        assert_eq!(reorder_with_step_size(&v, 5), vec![5, 10, 15]);
    }

    #[test]
    fn reorder_with_step_size_recovers_from_invalid_step() {
        // step_size=0 → 退化到 10
        let v = vec![1, 2];
        assert_eq!(reorder_with_step_size(&v, 0), vec![10, 20]);
        let v = vec![1, 2];
        assert_eq!(reorder_with_step_size(&v, -1), vec![10, 20]);
    }
}
