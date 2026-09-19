//! outsource 状态机
//!
//! 对应 Python myERP/statemachines/outsource_quote.py。
//! 实施约定：手写 enum + match 迁移表（`can_transition_to`），状态机不写 DB；
//! 事件日志由 service 在事务内统一插入。
//!
//! ## 2026-09-13 Phase 2 状态词汇
//!
//! - `DRAFT`：初始态，CLERK 录入中
//! - `SUBMITTED`：提交审核
//! - `APPROVED`：审批通过（可发送外协）
//! - `REJECTED`：审批拒绝（终态）
//! - `USED`：已用于发送（终态，2026-07-30 升级；当前 Phase 2 仅占位，service
//!   不自动迁；future work：approve 后发送时写入 USED 事件）
//!
//! 历史兼容词汇（OUTSOURCING / RECEIVED / BILLED / USED）保留以读取存量行，
//! 但 Phase 2 不再产生新流转（service 层不再调用 mark_outsourcing / mark_received
//! 等）。`from_str` 仍接受这些值以兼容旧数据。

use serde::{Deserialize, Serialize};

/// `t_outsource_quote.status` 取值（5 态）。
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutsourceQuoteStatus {
    #[serde(rename = "DRAFT")]
    DRAFT,
    #[serde(rename = "SUBMITTED")]
    SUBMITTED,
    #[serde(rename = "APPROVED")]
    APPROVED,
    #[serde(rename = "REJECTED")]
    REJECTED,
    #[serde(rename = "USED")]
    USED,
}

impl OutsourceQuoteStatus {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "DRAFT" => Self::DRAFT,
            "SUBMITTED" => Self::SUBMITTED,
            "APPROVED" => Self::APPROVED,
            "REJECTED" => Self::REJECTED,
            "USED" => Self::USED,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DRAFT => "DRAFT",
            Self::SUBMITTED => "SUBMITTED",
            Self::APPROVED => "APPROVED",
            Self::REJECTED => "REJECTED",
            Self::USED => "USED",
        }
    }

    /// 迁移白名单。
    ///
    /// Phase 2（2026-09-13）实现：
    /// - `DRAFT → SUBMITTED`（submit）
    /// - `SUBMITTED → APPROVED`（approve）
    /// - `SUBMITTED → REJECTED`（reject）
    ///
    /// 其它历史状态（OUTSOURCING / RECEIVED / BILLED）已下线，不再产生。
    /// `USED` 是终态（在 Python 中由发送时写入，Phase 2 暂未启用）。
    pub fn can_transition_to(self, to: Self) -> bool {
        use OutsourceQuoteStatus::*;
        matches!(
            (self, to),
            (DRAFT, SUBMITTED) | (SUBMITTED, APPROVED) | (SUBMITTED, REJECTED)
        )
    }

    /// 终态：APPROVED 在 Phase 2 不再视为终态（可被 USED）；
    /// REJECTED / USED 是终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::REJECTED | Self::USED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_statuses() {
        let all = [
            OutsourceQuoteStatus::DRAFT,
            OutsourceQuoteStatus::SUBMITTED,
            OutsourceQuoteStatus::APPROVED,
            OutsourceQuoteStatus::REJECTED,
            OutsourceQuoteStatus::USED,
        ];
        for s in all {
            assert_eq!(OutsourceQuoteStatus::from_str(s.as_str()), Some(s));
        }
    }

    #[test]
    fn from_str_unknown() {
        assert!(OutsourceQuoteStatus::from_str("OUTSOURCING").is_none());
        assert!(OutsourceQuoteStatus::from_str("RECEIVED").is_none());
        assert!(OutsourceQuoteStatus::from_str("BILLED").is_none());
        assert!(OutsourceQuoteStatus::from_str("UNKNOWN").is_none());
        assert!(OutsourceQuoteStatus::from_str("").is_none());
    }

    #[test]
    fn allowed_transitions() {
        assert!(OutsourceQuoteStatus::DRAFT.can_transition_to(OutsourceQuoteStatus::SUBMITTED));
        assert!(OutsourceQuoteStatus::SUBMITTED.can_transition_to(OutsourceQuoteStatus::APPROVED));
        assert!(OutsourceQuoteStatus::SUBMITTED.can_transition_to(OutsourceQuoteStatus::REJECTED));
    }

    #[test]
    fn disallowed_transitions() {
        // 自环非法
        for s in [
            OutsourceQuoteStatus::DRAFT,
            OutsourceQuoteStatus::SUBMITTED,
            OutsourceQuoteStatus::APPROVED,
            OutsourceQuoteStatus::REJECTED,
            OutsourceQuoteStatus::USED,
        ] {
            assert!(!s.can_transition_to(s), "{s:?} 自环必须拒绝");
        }
        // DRAFT → APPROVED / REJECTED 非法（必须先 SUBMITTED）
        assert!(!OutsourceQuoteStatus::DRAFT.can_transition_to(OutsourceQuoteStatus::APPROVED));
        assert!(!OutsourceQuoteStatus::DRAFT.can_transition_to(OutsourceQuoteStatus::REJECTED));
        // APPROVED → 任何其它都非法（终端在 Phase 2 实现里视为允许流转到 USED，
        // 但 Phase 2 不实现 USED 流转；保持白名单严格）
        assert!(!OutsourceQuoteStatus::APPROVED.can_transition_to(OutsourceQuoteStatus::USED));
        assert!(!OutsourceQuoteStatus::APPROVED.can_transition_to(OutsourceQuoteStatus::REJECTED));
        // REJECTED → 任何都非法（终态）
        assert!(!OutsourceQuoteStatus::REJECTED.can_transition_to(OutsourceQuoteStatus::DRAFT));
        assert!(!OutsourceQuoteStatus::REJECTED.can_transition_to(OutsourceQuoteStatus::SUBMITTED));
        // USED → 任何都非法（终态）
        assert!(!OutsourceQuoteStatus::USED.can_transition_to(OutsourceQuoteStatus::DRAFT));
    }

    #[test]
    fn terminal_states() {
        assert!(!OutsourceQuoteStatus::DRAFT.is_terminal());
        assert!(!OutsourceQuoteStatus::SUBMITTED.is_terminal());
        assert!(!OutsourceQuoteStatus::APPROVED.is_terminal());
        assert!(OutsourceQuoteStatus::REJECTED.is_terminal());
        assert!(OutsourceQuoteStatus::USED.is_terminal());
    }
}
