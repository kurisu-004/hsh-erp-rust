//! assembly 状态机
//!
//! 对应 Python myERP/statemachines/assembly_statemachine.py。
//!
//! 状态转移白名单（参考 part 状态机模式）：
//! - PENDING    → IN_PROCESS | CANCELLED
//! - IN_PROCESS → COMPLETED  | CANCELLED
//! - COMPLETED  → 终态（self-loop / 反向 / 跨度过渡均拒绝）
//! - CANCELLED  → 终态
//!
//! 注意：与 part 域不同，assembly 不走 inspection / 状态机扩展点，统一在此 enum 表达。

use serde::{Deserialize, Serialize};

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssemblyStatus {
    PENDING,
    IN_PROCESS,
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
            "COMPLETED" => Some(Self::COMPLETED),
            "CANCELLED" => Some(Self::CANCELLED),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PENDING => "PENDING",
            Self::IN_PROCESS => "IN_PROCESS",
            Self::COMPLETED => "COMPLETED",
            Self::CANCELLED => "CANCELLED",
        }
    }

    pub fn can_transition_to(self, to: Self) -> bool {
        use AssemblyStatus::*;
        matches!(
            (self, to),
            (PENDING, IN_PROCESS) | (PENDING, CANCELLED)
                | (IN_PROCESS, COMPLETED) | (IN_PROCESS, CANCELLED)
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
    }

    #[test]
    fn disallowed_self_loop() {
        use AssemblyStatus::*;
        assert!(!PENDING.can_transition_to(PENDING));
        assert!(!IN_PROCESS.can_transition_to(IN_PROCESS));
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
/// `service/_assembly_rollup.py::recompute_assembly_status`, collapsed to
/// Rust's 4-state AssemblyStatus enum).
///
/// Rules (in order):
/// 1. Empty input → `None` (caller decides no-op).
/// 2. All children CANCELLED → `Some(CANCELLED)`.
/// 3. All non-CANCELLED children COMPLETED → `Some(COMPLETED)`.
/// 4. Else `min(progress[child])` over non-terminal, non-cancelled → PENDING if 0 else IN_PROCESS.
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
    Some(if min_progress == 0 { PENDING } else { IN_PROCESS })
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
    fn all_inspection_or_above_returns_in_process() {
        assert_eq!(
            compute_assembly_target(["INSPECTION", "READY_TO_SHIP", "DELIVERED"]),
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
}
