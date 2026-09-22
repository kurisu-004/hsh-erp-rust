//! part 域 SQL 杂项 helper（2026-09-22 PR2 拆分原 sql.rs）
//!
//! ## 文件拆分原则（PR2 约定）
//! 按"主表归属"切分；函数体、签名、可见性、async 修饰全部保留；
//! **零 SQL 文本变化**（sqlx prepare 哈希一致）。
//!
//! ## 承载内容
//! - `scale_qty` —— §3.3 缩放公式纯函数（与 SQL
//!   `GREATEST(1, ROUND(quantity::numeric * new_qty / old_qty))` 严格一致）
//! - 单元测试 —— 整数倍缩放 / 四舍五入 / 下限 1 / 防御
//!
//! ## 备注
//! 本文件不依赖任何 sqlx 表结构（纯数值函数 + 单测）；sqlx prepare 不关心本文件。
//! 拆分目的：把单测从 `part_sql.rs` 的 SQL 流程中隔离，便于 cargo test 快速跑。

/// §3.3 — 套数缩放公式的纯函数表达（与 `scale_children_quantity` SQL
/// `GREATEST(1, ROUND(quantity::numeric * new_qty / old_qty))` 必须严格一致）。
///
/// 返回 `Some(new_child_qty)`；`old_qty <= 0` 或 `new_qty <= 0` → `None`
/// （service 层在这种情况跳过缩放，避免除零 / 反向缩放）。
///
/// 约定：
/// - 四舍五入方向：half-away-from-zero（与 PostgreSQL `ROUND(numeric)` 默认一致）。
/// - 下限：1（与 SQL `GREATEST(1, ...)` 一致）。
/// - 不追溯调整 `t_part_batch.quantity`（D1 决策）。
pub fn scale_qty(child_qty: i32, old_qty: i32, new_qty: i32) -> Option<i32> {
    if old_qty <= 0 || new_qty <= 0 {
        return None;
    }
    let raw = (child_qty as f64) * (new_qty as f64) / (old_qty as f64);
    let rounded = raw.round() as i32;
    Some(rounded.max(1))
}

#[cfg(test)]
mod tests {
    use super::scale_qty;

    /// 整数倍缩放：child=3, old=1, new=2 → round(6.0)=6
    #[test]
    fn scale_qty_integer_multiple() {
        assert_eq!(scale_qty(3, 1, 2), Some(6));
        assert_eq!(scale_qty(5, 1, 2), Some(10));
        assert_eq!(scale_qty(4, 2, 4), Some(8));
    }

    /// 四舍五入边界：child=1, old=3, new=5 → 1*5/3 ≈ 1.666 → round=2
    #[test]
    fn scale_qty_rounds_half_away_from_zero() {
        // 1.666... → 2
        assert_eq!(scale_qty(1, 3, 5), Some(2));
        // 2.666... → 3
        assert_eq!(scale_qty(2, 3, 4), Some(3));
        // 1.5 → 2（half away from zero）
        assert_eq!(scale_qty(3, 2, 1), Some(2));
        // 0.5 → 1（half away from zero，命中下限 1）
        assert_eq!(scale_qty(1, 2, 1), Some(1));
    }

    /// 下限 1：child_qty=1, old=10, new=3 → 0.3 → round=0 → GREATEST 1 = 1
    #[test]
    fn scale_qty_clamps_to_one() {
        assert_eq!(scale_qty(1, 10, 3), Some(1));
        assert_eq!(scale_qty(1, 100, 1), Some(1));
        // child=0 不可能（service 层会校验），但 formula 仍要正确处理
        assert_eq!(scale_qty(0, 10, 3), Some(1));
    }

    /// 防御：old_qty <= 0 / new_qty <= 0 → None（跳过缩放）
    #[test]
    fn scale_qty_skips_on_non_positive() {
        assert_eq!(scale_qty(3, 0, 2), None, "old_qty=0 应跳过");
        assert_eq!(scale_qty(3, -1, 2), None, "old_qty=-1 应跳过");
        assert_eq!(scale_qty(3, 1, 0), None, "new_qty=0 应跳过");
        assert_eq!(scale_qty(3, 1, -2), None, "new_qty=-2 应跳过");
    }
}
