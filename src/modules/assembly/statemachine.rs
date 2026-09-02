//! assembly 状态机
//!
//! 对应 Python myERP/statemachines/assembly_statemachine.py（2026-08-03 起 7 态扩展）。
//!
//! 状态转移白名单：
//! - PENDING        → IN_PROCESS | CANCELLED
//! - IN_PROCESS     → INSPECTION | COMPLETED | CANCELLED
//! - INSPECTION     → READY_TO_SHIP | CANCELLED
//! - READY_TO_SHIP  → DELIVERED | CANCELLED
//! - DELIVERED      → COMPLETED | CANCELLED
//! - COMPLETED      → 终态（self-loop / 反向 / 跨度过渡均拒绝）
//! - CANCELLED      → 终态
//!
//! 2026-09 Rust 端对齐 Python 的 7 态：新增 INSPECTION / READY_TO_SHIP / DELIVERED。
//! 与 part 域不同——assembly 的 rollup 跟随子件进度跨越 INSPECTION / READY_TO_SHIP / DELIVERED。

use serde::{Deserialize, Serialize};

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssemblyStatus {
    PENDING,
    IN_PROCESS,
    /// 2026-09 扩展：任一子件进入 INSPECTION 时父件跟随；对齐 Python `AssemblyStatus.INSPECTION`。
    INSPECTION,
    /// 2026-09 扩展：任一子件进入 READY_TO_SHIP 时父件跟随；对齐 Python。
    READY_TO_SHIP,
    /// 2026-09 扩展：任一子件进入 DELIVERED 时父件跟随；对齐 Python。
    DELIVERED,
    COMPLETED,
    CANCELLED,
}

impl AssemblyStatus {
    /// Domain-specific parser: returns `None` for unknown labels (vs. `FromStr`'s `Result<Self, Infallible>`).
    /// Different from std `str::parse::<Self>()` — not a public API surface.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "PENDING" => Some(Self::PENDING),
            "IN_PROCESS" => Some(Self::IN_PROCESS),
            "INSPECTION" => Some(Self::INSPECTION),
            "READY_TO_SHIP" => Some(Self::READY_TO_SHIP),
            "DELIVERED" => Some(Self::DELIVERED),
            "COMPLETED" => Some(Self::COMPLETED),
            "CANCELLED" => Some(Self::CANCELLED),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PENDING => "PENDING",
            Self::IN_PROCESS => "IN_PROCESS",
            Self::INSPECTION => "INSPECTION",
            Self::READY_TO_SHIP => "READY_TO_SHIP",
            Self::DELIVERED => "DELIVERED",
            Self::COMPLETED => "COMPLETED",
            Self::CANCELLED => "CANCELLED",
        }
    }

    pub fn can_transition_to(self, to: Self) -> bool {
        use AssemblyStatus::*;
        matches!(
            (self, to),
            // 原有：4 条
            (PENDING, IN_PROCESS) | (PENDING, CANCELLED)
                | (IN_PROCESS, COMPLETED) | (IN_PROCESS, CANCELLED)
            // 2026-09 新增：跟随子件派生态 + 取消
                | (IN_PROCESS, INSPECTION)
                | (INSPECTION, READY_TO_SHIP)
                | (READY_TO_SHIP, DELIVERED)
                | (DELIVERED, COMPLETED)
                | (INSPECTION, CANCELLED)
                | (READY_TO_SHIP, CANCELLED)
                | (DELIVERED, CANCELLED)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_statuses() {
        for s in [
            AssemblyStatus::PENDING,
            AssemblyStatus::IN_PROCESS,
            AssemblyStatus::INSPECTION,
            AssemblyStatus::READY_TO_SHIP,
            AssemblyStatus::DELIVERED,
            AssemblyStatus::COMPLETED,
            AssemblyStatus::CANCELLED,
        ] {
            assert_eq!(AssemblyStatus::from_str(s.as_str()), Some(s));
        }
    }

    #[test]
    fn from_str_unknown() {
        assert_eq!(AssemblyStatus::from_str("UNKNOWN"), None);
        assert_eq!(AssemblyStatus::from_str(""), None);
    }

    #[test]
    fn allowed_transitions() {
        use AssemblyStatus::*;
        assert!(PENDING.can_transition_to(IN_PROCESS));
        assert!(PENDING.can_transition_to(CANCELLED));
        assert!(IN_PROCESS.can_transition_to(COMPLETED));
        assert!(IN_PROCESS.can_transition_to(CANCELLED));
        assert!(IN_PROCESS.can_transition_to(INSPECTION));
        assert!(INSPECTION.can_transition_to(READY_TO_SHIP));
        assert!(READY_TO_SHIP.can_transition_to(DELIVERED));
        assert!(DELIVERED.can_transition_to(COMPLETED));
        assert!(INSPECTION.can_transition_to(CANCELLED));
        assert!(READY_TO_SHIP.can_transition_to(CANCELLED));
    }

    #[test]
    fn disallowed_self_loop() {
        use AssemblyStatus::*;
        assert!(!PENDING.can_transition_to(PENDING));
        assert!(!IN_PROCESS.can_transition_to(IN_PROCESS));
        assert!(!INSPECTION.can_transition_to(INSPECTION));
        assert!(!READY_TO_SHIP.can_transition_to(READY_TO_SHIP));
        assert!(!DELIVERED.can_transition_to(DELIVERED));
        assert!(!COMPLETED.can_transition_to(COMPLETED));
        assert!(!CANCELLED.can_transition_to(CANCELLED));
    }

    #[test]
    fn disallowed_terminal_to_active() {
        use AssemblyStatus::*;
        assert!(!COMPLETED.can_transition_to(IN_PROCESS));
        assert!(!COMPLETED.can_transition_to(PENDING));
        assert!(!CANCELLED.can_transition_to(PENDING));
        assert!(!CANCELLED.can_transition_to(IN_PROCESS));
    }

    #[test]
    fn disallowed_skip() {
        use AssemblyStatus::*;
        assert!(!PENDING.can_transition_to(COMPLETED));
        assert!(!IN_PROCESS.can_transition_to(PENDING));
        assert!(!INSPECTION.can_transition_to(COMPLETED));
        assert!(!INSPECTION.can_transition_to(DELIVERED));
    }

    #[test]
    fn allowed_cancel_from_all_states() {
        use AssemblyStatus::*;
        // 5 条 cancel 边：PENDING / IN_PROCESS / INSPECTION / READY_TO_SHIP / DELIVERED → CANCELLED
        assert!(PENDING.can_transition_to(CANCELLED));
        assert!(IN_PROCESS.can_transition_to(CANCELLED));
        assert!(INSPECTION.can_transition_to(CANCELLED));
        assert!(READY_TO_SHIP.can_transition_to(CANCELLED));
        assert!(DELIVERED.can_transition_to(CANCELLED));
        // COMPLETED / CANCELLED 不可再 cancel
        assert!(!COMPLETED.can_transition_to(CANCELLED));
        assert!(!CANCELLED.can_transition_to(CANCELLED));
    }
}

/// Part status → "progress rank" (mirrors Python ROLLUP_PROGRESS).
/// Unknown → 2 (IN_PROCESS-equivalent), safe default.
fn part_status_progress(s: &str) -> u8 {
    match s {
        "PENDING" => 0,
        "PROGRAMMING" => 1,
        "IN_PROCESS" | "REPAIRING" => 2,
        "OUTSOURCE" => 3,
        "INSPECTION" => 4,
        "READY_TO_SHIP" => 5,
        "DELIVERED" => 6,
        _ => 2,
    }
}

/// Aggregate child statuses into target assembly status (mirrors Python
/// `service/_assembly_rollup.py::recompute_assembly_status` and its
/// `ASSEMBLY_ROLLUP_TARGET` dict; 2026-09 Rust 端对齐 7 态）。
///
/// Rules (in order):
/// 1. Empty input → `None` (caller decides no-op).
/// 2. All children CANCELLED → `Some(CANCELLED)`.
/// 3. All non-CANCELLED children COMPLETED → `Some(COMPLETED)`.
/// 4. Else `min(progress[child])` over non-terminal, non-cancelled → 依
///    `part_status_progress` 0..6 映射到 7 态之一（PENDING / IN_PROCESS /
///    INSPECTION / READY_TO_SHIP / DELIVERED），防御兜底 `IN_PROCESS`。
pub fn compute_assembly_target<'a>(
    children_statuses: impl IntoIterator<Item = &'a str>,
) -> Option<AssemblyStatus> {
    use AssemblyStatus::*;
    let statuses: Vec<&str> = children_statuses.into_iter().collect();
    if statuses.is_empty() {
        return None;
    }
    let non_cancelled: Vec<&&str> = statuses.iter().filter(|s| **s != "CANCELLED").collect();
    if non_cancelled.is_empty() {
        return Some(CANCELLED);
    }
    let non_terminal: Vec<&&str> = non_cancelled
        .iter()
        .copied()
        .filter(|s| **s != "COMPLETED")
        .collect();
    if non_terminal.is_empty() {
        return Some(COMPLETED);
    }
    let min_progress = non_terminal
        .iter()
        .map(|s| part_status_progress(s))
        .min()
        .unwrap();
    Some(match min_progress {
        0 => PENDING,
        1 => IN_PROCESS,
        2 => IN_PROCESS,
        3 => IN_PROCESS,
        4 => INSPECTION,
        5 => READY_TO_SHIP,
        6 => DELIVERED,
        _ => IN_PROCESS, // 防御兜底：未知子件 progress
    })
}

#[cfg(test)]
mod rollup_tests {
    use super::*;

    #[test]
    fn empty_returns_none() {
        assert_eq!(compute_assembly_target(Vec::<&str>::new()), None);
        assert_eq!(compute_assembly_target([""; 0]), None);
    }

    #[test]
    fn all_cancelled_returns_cancelled() {
        assert_eq!(compute_assembly_target(["CANCELLED", "CANCELLED"]), Some(AssemblyStatus::CANCELLED));
    }

    #[test]
    fn all_completed_non_cancelled_returns_completed() {
        assert_eq!(compute_assembly_target(["COMPLETED", "COMPLETED"]), Some(AssemblyStatus::COMPLETED));
    }

    #[test]
    fn cancelled_ignored_others_completed_returns_completed() {
        assert_eq!(compute_assembly_target(["COMPLETED", "CANCELLED"]), Some(AssemblyStatus::COMPLETED));
    }

    #[test]
    fn single_pending_returns_pending() {
        assert_eq!(compute_assembly_target(["PENDING"]), Some(AssemblyStatus::PENDING));
    }

    #[test]
    fn mixed_min_progress_zero_returns_pending() {
        assert_eq!(
            compute_assembly_target(["PENDING", "IN_PROCESS", "DELIVERED"]),
            Some(AssemblyStatus::PENDING)
        );
    }

    #[test]
    fn min_progress_one_returns_in_process() {
        assert_eq!(
            compute_assembly_target(["PROGRAMMING", "DELIVERED"]),
            Some(AssemblyStatus::IN_PROCESS)
        );
    }

    #[test]
    fn cancelled_excluded_from_min() {
        // Without exclusion: min = CANCELLED. With exclusion: min = IN_PROCESS → IN_PROCESS.
        assert_eq!(
            compute_assembly_target(["CANCELLED", "IN_PROCESS", "INSPECTION"]),
            Some(AssemblyStatus::IN_PROCESS)
        );
    }

    #[test]
    fn mixed_terminal_non_terminal_returns_in_process() {
        assert_eq!(
            compute_assembly_target(["COMPLETED", "IN_PROCESS"]),
            Some(AssemblyStatus::IN_PROCESS)
        );
    }

    #[test]
    fn mixed_cancelled_completed_in_process_returns_in_process() {
        assert_eq!(
            compute_assembly_target(["CANCELLED", "COMPLETED", "IN_PROCESS"]),
            Some(AssemblyStatus::IN_PROCESS)
        );
    }

    #[test]
    fn min_progress_four_returns_inspection() {
        assert_eq!(
            compute_assembly_target(["INSPECTION", "DELIVERED"]),
            Some(AssemblyStatus::INSPECTION)
        );
    }

    #[test]
    fn min_progress_five_returns_ready_to_ship() {
        assert_eq!(
            compute_assembly_target(["READY_TO_SHIP", "DELIVERED"]),
            Some(AssemblyStatus::READY_TO_SHIP)
        );
    }

    #[test]
    fn min_progress_six_returns_delivered() {
        assert_eq!(
            compute_assembly_target(["DELIVERED", "DELIVERED"]),
            Some(AssemblyStatus::DELIVERED)
        );
    }
}
