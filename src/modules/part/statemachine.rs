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

// ===== rollup 派生列投影（PR-B2 重新加回，2026-09-11） =====
//
//  part/service/rollup.rs 的 `sync_from_batch_change` 需要纯函数聚合
//  `t_part_batch.status` → `t_part.status` + 派生列，避免引入完整 TPartBatch
//  让纯函数测试受阻。仅暴露 `BatchForRollup` + `compute_part_target` 两个 pub 工具。

use chrono::NaiveDateTime;

/// rollup 用的批次最小投影（仅 5 列）。
#[derive(Debug, Clone)]
pub struct BatchForRollup {
    pub status: String,
    pub location: Option<String>,
    pub current_holder_id: Option<i64>,
    pub next_process_id: Option<i64>,
    pub placed_at: Option<NaiveDateTime>,
}

/// rollup target 派生列。
#[derive(Debug, Clone)]
pub struct PartTarget {
    pub status: String,
    pub location: Option<String>,
    pub current_holder_id: Option<i64>,
    pub next_process_id: Option<i64>,
    pub placed_at: Option<NaiveDateTime>,
}

/// 从 part 的全部活跃批次聚合 part 的 target 派生列。
///
/// 聚合规则（PR-B2 简化版，2026-09-11 重新加回）：
/// 1. 若任一批次 `status='DELIVERED'` ⇒ part.status = 'DELIVERED'
/// 2. 若任一批次 `status='READY_TO_SHIP'` ⇒ part.status = 'READY_TO_SHIP'
/// 3. 若任一批次 `status='INSPECTION'` ⇒ part.status = 'INSPECTION'
/// 4. 若任一批次 `status='REPAIRING'` ⇒ part.status = 'REPAIRING'
/// 5. 若任一批次 `status='OUTSOURCE'` ⇒ part.status = 'OUTSOURCE'
/// 6. 若全部批次均 `IN_PROCESS` ⇒ part.status = 'IN_PROCESS'
/// 7. 若全部批次均 `PENDING` ⇒ part.status = 'PENDING'
/// 8. 全部批次状态一致 ⇒ 沿用
/// 9. 空集 ⇒ None
///
/// 优先级：终态 > 进行态；INSPECTION > REPAIRING > OUTSOURCE > IN_PROCESS > PENDING。
///
/// `location` / `current_holder_id` / `next_process_id` / `placed_at` 取最慢批次
/// （按 placed_at ASC NULLS LAST，缺 placed_at 的批次放最后）。
pub fn compute_part_target(batches: &[BatchForRollup]) -> Option<PartTarget> {
    if batches.is_empty() {
        return None;
    }
    // 优先级字符串：覆盖的优先级（值越大优先级越高）
    let priority = |s: &str| match s {
        "DELIVERED" => 6,
        "READY_TO_SHIP" => 5,
        "INSPECTION" => 4,
        "REPAIRING" => 3,
        "OUTSOURCE" => 3,
        "IN_PROCESS" => 2,
        "PROGRAMMING" => 2,
        "PENDING" => 1,
        "COMPLETED" => 0,
        "CANCELLED" => 0,
        _ => 0,
    };

    let best = batches
        .iter()
        .max_by_key(|b| (priority(&b.status), b.id_sort_key()))
        .expect("batches non-empty");

    Some(PartTarget {
        status: best.status.clone(),
        location: best.location.clone(),
        current_holder_id: best.current_holder_id,
        next_process_id: best.next_process_id,
        placed_at: best.placed_at,
    })
}

impl BatchForRollup {
    /// 排序键：placed_at ASC NULLS LAST，缺 placed_at 排最后。
    /// 用于 `compute_part_target` 取"最慢批次"。
    fn id_sort_key(&self) -> (u8, i64) {
        match self.placed_at {
            Some(t) => (0, t.and_utc().timestamp()),
            None => (1, 0),
        }
    }
}

#[cfg(test)]
mod rollup_tests {
    use super::*;

    fn br(status: &str, placed_at: Option<NaiveDateTime>) -> BatchForRollup {
        BatchForRollup {
            status: status.to_string(),
            location: None,
            current_holder_id: None,
            next_process_id: None,
            placed_at,
        }
    }

    #[test]
    fn empty_returns_none() {
        assert!(compute_part_target(&[]).is_none());
    }

    #[test]
    fn single_batch_passthrough() {
        let t = compute_part_target(&[br("IN_PROCESS", None)]).unwrap();
        assert_eq!(t.status, "IN_PROCESS");
    }

    #[test]
    fn delivered_wins_over_in_process() {
        let t = compute_part_target(&[
            br("IN_PROCESS", None),
            br("DELIVERED", None),
        ])
        .unwrap();
        assert_eq!(t.status, "DELIVERED");
    }

    #[test]
    fn inspection_wins_over_in_process() {
        let t = compute_part_target(&[
            br("IN_PROCESS", None),
            br("INSPECTION", None),
        ])
        .unwrap();
        assert_eq!(t.status, "INSPECTION");
    }

    #[test]
    fn all_pending_returns_pending() {
        let t = compute_part_target(&[br("PENDING", None), br("PENDING", None)]).unwrap();
        assert_eq!(t.status, "PENDING");
    }
}
