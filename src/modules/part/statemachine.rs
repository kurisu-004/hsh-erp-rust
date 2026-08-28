//! part 状态机
//!
//! 对应 Python myERP/statemachines/part_state_machine.py。
//! 实施约定：手写 enum + match 迁移表（`can_transition_to`），状态机不写 DB；
//! 事件日志由 service 在事务内统一插入。
//!
//! 状态词汇与 `migrations/20260811100005_005_create_part_tables.sql` 中
//! `t_part.status` 列语义保持一致（PENDING 默认；INSPECTION 为待检，
//! READY_TO_SHIP 为待出 / 待装车）。

use serde::{Deserialize, Serialize};

/// `t_part.status` 取值（与 migration 005 字段语义对齐）。
///
/// to-XXX 流（to_inspection / to_ship / to_process）按状态机白名单放行；
/// 其它合法迁移留到后续 PR 补齐。当前 `can_transition_to` 严格按白名单放行。
///
/// `allow(non_camel_case_types)`：变体名沿用 DB 列值（`IN_PROCESS` /
/// `READY_TO_SHIP` 等），通过 `#[serde(rename = "...")]` 控制 JSON 序列化。
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartStatus {
    #[serde(rename = "PENDING")]
    PENDING,
    #[serde(rename = "PROGRAMMING")]
    PROGRAMMING,
    #[serde(rename = "IN_PROCESS")]
    IN_PROCESS,
    #[serde(rename = "INSPECTION")]
    INSPECTION,
    #[serde(rename = "READY_TO_SHIP")]
    READY_TO_SHIP,
    #[serde(rename = "DELIVERED")]
    DELIVERED,
    #[serde(rename = "REPAIRING")]
    REPAIRING,
    #[serde(rename = "OUTSOURCE")]
    OUTSOURCE,
    #[serde(rename = "COMPLETED")]
    COMPLETED,
    #[serde(rename = "CANCELLED")]
    CANCELLED,
}

impl PartStatus {
    /// 由 DB 字符串反序列化为枚举；未知值返回 `None`。
    ///
    /// 故意不实现 `std::str::FromStr`：DB 词汇集是白名单（10 个值），
    /// `Option<Self>` 比 `Result<Self, _>` 更贴合 caller 习惯（直接 `?` 抛
    /// `AppError::biz(code::BIZ_INVALID_STATUS, ...)`）。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "PENDING" => Self::PENDING,
            "PROGRAMMING" => Self::PROGRAMMING,
            "IN_PROCESS" => Self::IN_PROCESS,
            "INSPECTION" => Self::INSPECTION,
            "READY_TO_SHIP" => Self::READY_TO_SHIP,
            "DELIVERED" => Self::DELIVERED,
            "REPAIRING" => Self::REPAIRING,
            "OUTSOURCE" => Self::OUTSOURCE,
            "COMPLETED" => Self::COMPLETED,
            "CANCELLED" => Self::CANCELLED,
            _ => return None,
        })
    }

    /// 序列化为 DB 字符串（与 migration 005 列值一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PENDING => "PENDING",
            Self::PROGRAMMING => "PROGRAMMING",
            Self::IN_PROCESS => "IN_PROCESS",
            Self::INSPECTION => "INSPECTION",
            Self::READY_TO_SHIP => "READY_TO_SHIP",
            Self::DELIVERED => "DELIVERED",
            Self::REPAIRING => "REPAIRING",
            Self::OUTSOURCE => "OUTSOURCE",
            Self::COMPLETED => "COMPLETED",
            Self::CANCELLED => "CANCELLED",
        }
    }

    /// 迁移白名单（共 14 个合法迁移）。
    ///
    /// to-XXX 流放行：
    /// - `INSPECTION → READY_TO_SHIP`：to_ship 路径
    /// - `INSPECTION → IN_PROCESS`：to_process 路径
    /// - `PROGRAMMING / PENDING / IN_PROCESS → INSPECTION`：to_inspection 路径（任意源状态）
    ///
    /// PR-CRUD 新增：
    /// - `READY_TO_SHIP → DELIVERED` (deliver)
    /// - `DELIVERED → COMPLETED` (complete)
    /// - `PENDING/PROGRAMMING/INSPECTION/READY_TO_SHIP/DELIVERED → CANCELLED` (cancel)
    /// - `IN_PROCESS → REPAIRING` (start-repair)
    ///
    /// 扫描返修新增（scan-route B 组 to-inspection）：
    /// - `REPAIRING → INSPECTION` (to-inspection：返修完成 → 重新送检)
    ///
    /// IN_PROCESS+WORKER 拒绝 / IN_PROCESS+非 PRODUCTION_SHELF 拒绝走
    /// service 层组合校验（仿 myERP `service/part.py:4140-4164`），不污染
    /// 状态机白名单。
    pub fn can_transition_to(self, to: Self) -> bool {
        use PartStatus::*;
        matches!(
            (self, to),
            // 既有（保留）
            (INSPECTION, READY_TO_SHIP)
                | (INSPECTION, IN_PROCESS)
                | (PROGRAMMING, INSPECTION)
                | (PENDING, INSPECTION)
                | (IN_PROCESS, INSPECTION)
            // PR-CRUD 新增
                | (READY_TO_SHIP, DELIVERED)        // deliver
                | (DELIVERED, COMPLETED)            // complete
                | (PENDING, CANCELLED)              // cancel
                | (PROGRAMMING, CANCELLED)
                | (INSPECTION, CANCELLED)
                | (READY_TO_SHIP, CANCELLED)
                | (DELIVERED, CANCELLED)
                | (IN_PROCESS, REPAIRING)           // start-repair
            // 扫描返修新增（scan-route B 组走 to-inspection）
                | (REPAIRING, INSPECTION)            // 返修完成 → 重新送检（B 组走 to-inspection）
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_statuses() {
        let all = [
            PartStatus::PENDING,
            PartStatus::PROGRAMMING,
            PartStatus::IN_PROCESS,
            PartStatus::INSPECTION,
            PartStatus::READY_TO_SHIP,
            PartStatus::DELIVERED,
            PartStatus::REPAIRING,
            PartStatus::OUTSOURCE,
            PartStatus::COMPLETED,
            PartStatus::CANCELLED,
        ];
        for s in all {
            let round = PartStatus::from_str(s.as_str()).expect("round-trip");
            assert_eq!(round, s);
        }
    }

    #[test]
    fn from_str_unknown() {
        assert!(PartStatus::from_str("UNKNOWN").is_none());
        assert!(PartStatus::from_str("").is_none());
    }

    #[test]
    fn allowed_transitions() {
        assert!(PartStatus::INSPECTION.can_transition_to(PartStatus::READY_TO_SHIP));
        assert!(PartStatus::PROGRAMMING.can_transition_to(PartStatus::INSPECTION));
    }

    #[test]
    fn disallowed_transitions_default_false() {
        assert!(!PartStatus::PENDING.can_transition_to(PartStatus::READY_TO_SHIP));
        assert!(!PartStatus::DELIVERED.can_transition_to(PartStatus::INSPECTION));
        assert!(!PartStatus::READY_TO_SHIP.can_transition_to(PartStatus::INSPECTION));
    }

    #[test]
    fn allowed_transitions_to_inspection() {
        assert!(PartStatus::INSPECTION.can_transition_to(PartStatus::READY_TO_SHIP));
        assert!(PartStatus::PROGRAMMING.can_transition_to(PartStatus::INSPECTION));
        // to_inspection 流新增
        assert!(PartStatus::PENDING.can_transition_to(PartStatus::INSPECTION));
        assert!(PartStatus::IN_PROCESS.can_transition_to(PartStatus::INSPECTION));
        // to_process 流新增
        assert!(PartStatus::INSPECTION.can_transition_to(PartStatus::IN_PROCESS));
    }

    #[test]
    fn disallowed_transitions_to_inspection_rejects() {
        // 自环非法
        assert!(!PartStatus::INSPECTION.can_transition_to(PartStatus::INSPECTION));
        // 反向非法
        assert!(!PartStatus::READY_TO_SHIP.can_transition_to(PartStatus::INSPECTION));
        // 跨度过大
        assert!(!PartStatus::PENDING.can_transition_to(PartStatus::READY_TO_SHIP));
        assert!(!PartStatus::IN_PROCESS.can_transition_to(PartStatus::READY_TO_SHIP));
    }

    #[test]
    fn allowed_transitions_repairing_to_inspection() {
        assert!(PartStatus::REPAIRING.can_transition_to(PartStatus::INSPECTION));
    }

    #[test]
    fn disallowed_transitions_repairing_rejects() {
        assert!(!PartStatus::REPAIRING.can_transition_to(PartStatus::READY_TO_SHIP));
        assert!(!PartStatus::REPAIRING.can_transition_to(PartStatus::COMPLETED));
        assert!(!PartStatus::REPAIRING.can_transition_to(PartStatus::IN_PROCESS));
    }

    #[test]
    fn allowed_transitions_lifecycle() {
        assert!(PartStatus::READY_TO_SHIP.can_transition_to(PartStatus::DELIVERED));
        assert!(PartStatus::DELIVERED.can_transition_to(PartStatus::COMPLETED));
        for s in [
            PartStatus::PENDING,
            PartStatus::PROGRAMMING,
            PartStatus::INSPECTION,
            PartStatus::READY_TO_SHIP,
            PartStatus::DELIVERED,
        ] {
            assert!(s.can_transition_to(PartStatus::CANCELLED), "from {s:?} should be cancellable");
        }
        assert!(PartStatus::IN_PROCESS.can_transition_to(PartStatus::REPAIRING));
    }

    #[test]
    fn disallowed_transitions_lifecycle_rejects() {
        assert!(!PartStatus::DELIVERED.can_transition_to(PartStatus::DELIVERED));
        assert!(!PartStatus::COMPLETED.can_transition_to(PartStatus::CANCELLED));
        assert!(!PartStatus::COMPLETED.can_transition_to(PartStatus::DELIVERED));
        assert!(!PartStatus::CANCELLED.can_transition_to(PartStatus::PENDING));
        assert!(!PartStatus::PENDING.can_transition_to(PartStatus::REPAIRING));
        assert!(!PartStatus::INSPECTION.can_transition_to(PartStatus::REPAIRING));
        assert!(!PartStatus::INSPECTION.can_transition_to(PartStatus::DELIVERED));
        assert!(!PartStatus::READY_TO_SHIP.can_transition_to(PartStatus::COMPLETED));
    }
}
