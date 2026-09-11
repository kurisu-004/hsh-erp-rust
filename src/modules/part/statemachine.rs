//! part 状态机
//!
//! 对应 Python myERP/statemachines/part_state_machine.py。
//! 实施约定：手写 enum + match 迁移表（`can_transition_to`），状态机不写 DB；
//! 事件日志由 service 在事务内统一插入。
//!
//! 状态词汇与 `migrations/20260811100005_005_create_part_tables.sql` 中
//! `t_part.status` 列语义保持一致（PENDING 默认；INSPECTION 为待检，
//! READY_TO_SHIP 为待出 / 待装车）。
//!
//! ## rollup 函数（part/assembly/batch 重构方案 §4.2 PR-B2）
//!
//! `part_status_progress` 与 `compute_part_target` 用于 batch → part 状态回
//! 流的 rollup 计算（与 assembly 域 `compute_assembly_target` 同构）。原
//! `part_status_progress` 私有定义在 `assembly/statemachine.rs`，提升为共享
//! 函数：assembly 端 `compute_assembly_target` 通过本模块导入。

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

// ===== rollup 共享函数（part/assembly/batch 重构方案 §4.2 PR-B2） =====

/// batch.status → progress rank（镜像 Python `ROLLUP_PROGRESS`）。
///
/// 序值：
/// - PENDING     = 0
/// - PROGRAMMING = 1
/// - IN_PROCESS  = 2（REPAIRING 同值，因返修仍在生产中）
/// - OUTSOURCE   = 3（外包是 IN_PROCESS 的延伸，独立 rank）
/// - INSPECTION  = 4
/// - READY_TO_SHIP = 5
/// - DELIVERED   = 6
///
/// 未知值 → 2（IN_PROCESS 等价，防御兜底）。
///
/// 提升为 pub：从 assembly 域 `compute_assembly_target` 复用，避免两个域各
/// 维护一份 progress 表导致漂移。
pub fn part_status_progress(s: &str) -> u8 {
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

/// `compute_part_target` 的输入投影（仅 rollup 所需列；避免引入完整 `TPartBatch`）。
#[derive(Debug, Clone, Default)]
pub struct BatchForRollup {
    pub status: String,
    pub location: Option<String>,
    pub current_holder_id: Option<i64>,
    pub next_process_id: Option<i64>,
    pub placed_at: Option<chrono::NaiveDateTime>,
}

/// `compute_part_target` 的输出：目标 status + 派生列（从最慢批次物化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRollupTarget {
    pub status: String,
    pub location: Option<String>,
    pub current_holder_id: Option<i64>,
    pub next_process_id: Option<i64>,
    pub placed_at: Option<chrono::NaiveDateTime>,
}

/// batch 集 → part rollup target（part/assembly/batch 重构方案 §4.2 步骤 1–6）。
///
/// 规则（与 assembly 端 `compute_assembly_target` 同构，但 part 与 batch 状态
/// 词汇相同，直接取 status 字符串）：
///
/// 1. 空集 → `None`（防御；caller 走 NoChange）
/// 2. 全部 CANCELLED → `status='CANCELLED'`；location/holder/process/placed_at
///    取首条（rollup 终态，派生列不影响业务语义）
/// 3. 非 CANCELLED 全部 COMPLETED → `status='COMPLETED'`；派生列取首条非
///    CANCELLED 批次（语义同上）
/// 4. 否则取「最慢批次」（min progress over non-terminal, non-cancelled）的
///    status + 派生列；progress 表见 `part_status_progress`
pub fn compute_part_target(batches: &[BatchForRollup]) -> Option<PartRollupTarget> {
    if batches.is_empty() {
        return None;
    }
    let non_cancelled: Vec<&BatchForRollup> =
        batches.iter().filter(|b| b.status != "CANCELLED").collect();
    if non_cancelled.is_empty() {
        let r = &batches[0];
        return Some(PartRollupTarget {
            status: "CANCELLED".to_string(),
            location: r.location.clone(),
            current_holder_id: r.current_holder_id,
            next_process_id: r.next_process_id,
            placed_at: r.placed_at,
        });
    }
    let non_terminal: Vec<&BatchForRollup> = non_cancelled
        .iter()
        .copied()
        .filter(|b| b.status != "COMPLETED")
        .collect();
    if non_terminal.is_empty() {
        let r = non_cancelled[0];
        return Some(PartRollupTarget {
            status: "COMPLETED".to_string(),
            location: r.location.clone(),
            current_holder_id: r.current_holder_id,
            next_process_id: r.next_process_id,
            placed_at: r.placed_at,
        });
    }
    // 取 min-progress 批次
    let min = non_terminal
        .iter()
        .min_by_key(|b| part_status_progress(&b.status))
        .copied()
        .unwrap(); // safety: non_terminal 至少有一条
    Some(PartRollupTarget {
        status: min.status.clone(),
        location: min.location.clone(),
        current_holder_id: min.current_holder_id,
        next_process_id: min.next_process_id,
        placed_at: min.placed_at,
    })
}

#[cfg(test)]
mod rollup_tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn batch(status: &str) -> BatchForRollup {
        BatchForRollup {
            status: status.to_string(),
            location: None,
            current_holder_id: None,
            next_process_id: None,
            placed_at: None,
        }
    }

    fn batch_with_loc(status: &str, loc: Option<&str>) -> BatchForRollup {
        BatchForRollup {
            status: status.to_string(),
            location: loc.map(str::to_string),
            current_holder_id: None,
            next_process_id: None,
            placed_at: None,
        }
    }

    #[test]
    fn empty_returns_none() {
        let v: Vec<BatchForRollup> = vec![];
        assert_eq!(compute_part_target(&v), None);
    }

    #[test]
    fn all_cancelled_returns_cancelled() {
        let v = vec![batch("CANCELLED"), batch("CANCELLED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "CANCELLED");
    }

    #[test]
    fn all_completed_non_cancelled_returns_completed() {
        let v = vec![batch("COMPLETED"), batch("COMPLETED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "COMPLETED");
    }

    #[test]
    fn cancelled_ignored_others_completed_returns_completed() {
        let v = vec![batch("COMPLETED"), batch("CANCELLED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "COMPLETED");
    }

    #[test]
    fn single_pending_returns_pending() {
        let r = compute_part_target(&[batch("PENDING")]).unwrap();
        assert_eq!(r.status, "PENDING");
    }

    #[test]
    fn min_progress_zero_pending() {
        let v = vec![batch("PENDING"), batch("IN_PROCESS"), batch("DELIVERED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "PENDING");
    }

    #[test]
    fn min_progress_one_programming_returns_programming() {
        let v = vec![batch("PROGRAMMING"), batch("DELIVERED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "PROGRAMMING");
    }

    #[test]
    fn cancelled_excluded_from_min() {
        let v = vec![batch("CANCELLED"), batch("IN_PROCESS"), batch("INSPECTION")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "IN_PROCESS");
    }

    #[test]
    fn mixed_terminal_non_terminal() {
        let v = vec![batch("COMPLETED"), batch("IN_PROCESS")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "IN_PROCESS");
    }

    #[test]
    fn mixed_cancelled_completed_in_process() {
        let v = vec![batch("CANCELLED"), batch("COMPLETED"), batch("IN_PROCESS")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "IN_PROCESS");
    }

    #[test]
    fn min_progress_four_inspection() {
        let v = vec![batch("INSPECTION"), batch("DELIVERED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "INSPECTION");
    }

    #[test]
    fn min_progress_five_ready_to_ship() {
        let v = vec![batch("READY_TO_SHIP"), batch("DELIVERED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "READY_TO_SHIP");
    }

    #[test]
    fn min_progress_six_delivered() {
        let v = vec![batch("DELIVERED"), batch("DELIVERED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "DELIVERED");
    }

    #[test]
    fn repairing_maps_to_in_process_progress() {
        assert_eq!(part_status_progress("REPAIRING"), 2);
        assert_eq!(part_status_progress("IN_PROCESS"), 2);
    }

    #[test]
    fn outsource_maps_to_progress_three() {
        assert_eq!(part_status_progress("OUTSOURCE"), 3);
    }

    #[test]
    fn unknown_status_defaults_to_in_process_progress() {
        assert_eq!(part_status_progress("UNKNOWN"), 2);
        assert_eq!(part_status_progress(""), 2);
    }

    #[test]
    fn min_progress_picks_repairing_over_in_process() {
        // REPAIRING 与 IN_PROCESS 同 progress (2)；min_by_key 在相等时取先出现者。
        // 这里验证混合情况下 REPAIRING 不会被错误地排除：
        let v = vec![batch("REPAIRING"), batch("DELIVERED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "REPAIRING");
    }

    #[test]
    fn min_progress_outsource_beats_delivered() {
        let v = vec![batch("OUTSOURCE"), batch("DELIVERED")];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "OUTSOURCE");
    }

    #[test]
    fn materializes_location_from_min_progress_batch() {
        let mut b_in_process = batch_with_loc("IN_PROCESS", Some("PRODUCTION_SHELF"));
        b_in_process.current_holder_id = Some(42);
        b_in_process.next_process_id = Some(7);
        let placed = NaiveDateTime::parse_from_str("2026-09-01T10:00:00", "%Y-%m-%dT%H:%M:%S")
            .unwrap();
        b_in_process.placed_at = Some(placed);
        let v = vec![b_in_process, batch_with_loc("DELIVERED", Some("OUTSOURCE_COMPANY"))];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "IN_PROCESS");
        assert_eq!(r.location.as_deref(), Some("PRODUCTION_SHELF"));
        assert_eq!(r.current_holder_id, Some(42));
        assert_eq!(r.next_process_id, Some(7));
        assert_eq!(r.placed_at, Some(placed));
    }

    #[test]
    fn all_cancelled_materializes_first_batch_loc() {
        let v = vec![
            batch_with_loc("CANCELLED", Some("OFFICE")),
            batch_with_loc("CANCELLED", Some("PRODUCTION_SHELF")),
        ];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "CANCELLED");
        assert_eq!(r.location.as_deref(), Some("OFFICE"));
    }
}
