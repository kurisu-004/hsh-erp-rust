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
