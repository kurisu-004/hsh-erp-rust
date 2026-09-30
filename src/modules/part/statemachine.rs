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
//! ## 2026-09-29 废弃 PROGRAMMING 进入路径
//!
//! `PartStatus::PROGRAMMING` 仍保留枚举值（历史数据兼容），但 `can_transition_to`
//! 删除了 `PENDING → PROGRAMMING` 与 `IN_PROCESS → PROGRAMMING` 两条入口。
//! 编程员现在通过 `release-from-programming` 唯一出路把已有 PROGRAMMING 批次
//! 消化到生产流；待编程一览改为基于 `t_process.is_cnc` 列的过滤（新进入路径
//! 是「直接在 PROCESS_CHAIN 步骤排 CNC step + 上架」）。
//!
//! 保留 4 条出口供历史数据消化：
//! - `PROGRAMMING → PENDING`（recall-to-pending，2026-09-13）
//! - `PROGRAMMING → IN_PROCESS`（release-from-programming，2026-09-13）
//! - `PROGRAMMING → INSPECTION`（to-inspection，PR-CRUD）
//! - `PROGRAMMING → CANCELLED`（cancel，PR-CRUD）
//!
//! `part_status_progress` 与 `compute_part_target` 用于 batch → part 状态回
//! 流的 rollup 计算（与 assembly 域 `compute_assembly_target` 同构）。原
//! `part_status_progress` 私有定义在 `assembly/statemachine.rs`，提升为共享
//! 函数：assembly 端 `compute_assembly_target` 通过本模块导入。
//!
//! 2026-09-29 废弃：PROGRAMMING 状态的进入路径 `PENDING → PROGRAMMING` 与
//! `IN_PROCESS → PROGRAMMING` 已从 `can_transition_to` 删除。保留 4 条出口：
//! `PROGRAMMING → PENDING` / `PROGRAMMING → IN_PROCESS` / `PROGRAMMING → INSPECTION` /
//! `PROGRAMMING → CANCELLED`。枚举值与 `as_str()` / `FromStr` 映射仍保留以
//! 兼容历史数据。

use serde::{Deserialize, Serialize};

/// `t_part.status` 取值（与 migration 005 字段语义对齐）。
///
/// to-XXX 流（to_inspection / to_ship / to_process）按状态机白名单放行；
/// 其它合法迁移留到后续 PR 补齐。当前 `can_transition_to` 严格按白名单放行。
///
/// **2026-09-29 废弃**：`PROGRAMMING` 进入路径已关闭（仅保留 4 条出口供历史数据消化）；
/// 编程员现在通过工艺链 + CNC step 直接进入 IN_PROCESS；待编程一览由
/// `t_process.is_cnc` 列驱动。
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

    /// 迁移白名单（2026-09-29：19 条合法迁移，PROGRAMMING 两条入口已废弃）。
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
    /// Phase 1（2026-09-13）补齐 14 端点：
    /// - `PENDING → IN_PROCESS`：place-on-shelf（ON_SHELF 在 DB 是 status=IN_PROCESS +
    ///   location=PRODUCTION_SHELF，service 层守 location；状态机仅做 status 白名单）
    /// - `IN_PROCESS → PENDING`：recall-to-pending（service 层要求 location=PRODUCTION_SHELF）
    /// - `PROGRAMMING → PENDING`：recall-to-pending
    /// - `PROGRAMMING → IN_PROCESS`：release-from-programming
    /// - `REPAIRING → IN_PROCESS`：complete-repair（落回生产架）
    /// - `OUTSOURCE → IN_PROCESS`：receive-from-outsource（回生产架）
    /// - `OUTSOURCE → INSPECTION`：receive-from-outsource-to-inspection
    /// - `PENDING → OUTSOURCE`：send-to-outsource（service 层也允许 IN_PROCESS 源走 OUTSOURCE；
    ///   状态机放行 PENDING → OUTSOURCE，service 层另守 IN_PROCESS→OUTSOURCE）
    /// - `INSPECTION → REPAIRING`：scan-inspect FAIL
    /// - `REPAIRING → CANCELLED`：cancel 路径
    /// - `OUTSOURCE → CANCELLED`：cancel 路径
    ///
    /// 2026-09-29 废弃：
    /// - 删除 `PENDING → PROGRAMMING`（原 send-to-programming；端点已下线）
    /// - 删除 `IN_PROCESS → PROGRAMMING`（原 recall-to-programming；端点已下线）
    /// - 保留 `PROGRAMMING → PENDING/IN_PROCESS/INSPECTION/CANCELLED` 共 4 条出口供
    ///   历史数据消化
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
            // Phase 1（2026-09-13）补齐
                | (PENDING, IN_PROCESS)              // place-on-shelf
                | (IN_PROCESS, PENDING)              // recall-to-pending（service 层守 location=PRODUCTION_SHELF）
                | (PROGRAMMING, PENDING)             // recall-to-pending
                | (PROGRAMMING, IN_PROCESS)          // release-from-programming
                | (REPAIRING, IN_PROCESS)            // complete-repair（落回生产架）
                | (OUTSOURCE, IN_PROCESS)            // receive-from-outsource（回生产架）
                | (OUTSOURCE, INSPECTION)            // receive-from-outsource-to-inspection
                | (PENDING, OUTSOURCE)               // send-to-outsource（DIRECT 路径从 PENDING 发）
                | (INSPECTION, REPAIRING)            // scan-inspect FAIL
                | (REPAIRING, CANCELLED)             // cancel 路径
                | (OUTSOURCE, CANCELLED) // cancel 路径
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
        // Phase 1 (2026-09-13): REPAIRING → IN_PROCESS 现在允许（complete-repair 落回生产架）
        // 由 `allowed_phase1_complete_repair_to_process` 单独断言；
        // 本测试仅保留 READY_TO_SHIP / COMPLETED 两个明确拒绝的断言。
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
            PartStatus::REPAIRING, // Phase 1 新增
            PartStatus::OUTSOURCE, // Phase 1 新增
        ] {
            assert!(
                s.can_transition_to(PartStatus::CANCELLED),
                "from {s:?} should be cancellable"
            );
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
        // Phase 1 (2026-09-13): INSPECTION → REPAIRING 现在允许（scan-inspect FAIL）；
        // 由 `allowed_phase1_scan_inspect_fail` 单独断言。
        assert!(!PartStatus::INSPECTION.can_transition_to(PartStatus::DELIVERED));
        assert!(!PartStatus::READY_TO_SHIP.can_transition_to(PartStatus::COMPLETED));
    }

    // ===== Phase 1（2026-09-13）扩展测试 =====

    #[test]
    fn allowed_phase1_place_on_shelf() {
        // PENDING → IN_PROCESS（place-on-shelf 入口）
        assert!(PartStatus::PENDING.can_transition_to(PartStatus::IN_PROCESS));
    }

    #[test]
    fn allowed_phase1_recall_to_pending() {
        // IN_PROCESS → PENDING（recall-to-pending；service 层守 location=PRODUCTION_SHELF）
        assert!(PartStatus::IN_PROCESS.can_transition_to(PartStatus::PENDING));
        // PROGRAMMING → PENDING
        assert!(PartStatus::PROGRAMMING.can_transition_to(PartStatus::PENDING));
    }

    #[test]
    fn allowed_phase1_release_from_programming() {
        // PROGRAMMING → IN_PROCESS（release-from-programming）
        assert!(PartStatus::PROGRAMMING.can_transition_to(PartStatus::IN_PROCESS));
    }

    #[test]
    fn allowed_phase1_complete_repair_to_process() {
        // REPAIRING → IN_PROCESS（complete-repair 落回生产架）
        assert!(PartStatus::REPAIRING.can_transition_to(PartStatus::IN_PROCESS));
    }

    #[test]
    fn disallowed_2026_09_29_programming_entry_removed() {
        // 2026-09-29：PROGRAMMING 状态废弃进入路径
        // - PENDING → PROGRAMMING（原 send-to-programming，端点已下线）
        // - IN_PROCESS → PROGRAMMING（原 recall-to-programming，端点已下线）
        assert!(
            !PartStatus::PENDING.can_transition_to(PartStatus::PROGRAMMING),
            "2026-09-29: PENDING → PROGRAMMING 已废弃"
        );
        assert!(
            !PartStatus::IN_PROCESS.can_transition_to(PartStatus::PROGRAMMING),
            "2026-09-29: IN_PROCESS → PROGRAMMING 已废弃"
        );
    }

    #[test]
    fn allowed_2026_09_29_programming_exit_preserved() {
        // 2026-09-29：PROGRAMMING 仍保留 4 条出口供历史数据消化
        assert!(PartStatus::PROGRAMMING.can_transition_to(PartStatus::PENDING));
        assert!(PartStatus::PROGRAMMING.can_transition_to(PartStatus::IN_PROCESS));
        assert!(PartStatus::PROGRAMMING.can_transition_to(PartStatus::INSPECTION));
        assert!(PartStatus::PROGRAMMING.can_transition_to(PartStatus::CANCELLED));
    }

    #[test]
    fn allowed_phase1_receive_from_outsource() {
        // OUTSOURCE → IN_PROCESS（receive-from-outsource 回生产架）
        assert!(PartStatus::OUTSOURCE.can_transition_to(PartStatus::IN_PROCESS));
        // OUTSOURCE → INSPECTION（receive-from-outsource-to-inspection）
        assert!(PartStatus::OUTSOURCE.can_transition_to(PartStatus::INSPECTION));
    }

    #[test]
    fn allowed_phase1_send_to_outsource() {
        // PENDING → OUTSOURCE（send-to-outsource；DIRECT 路径）
        assert!(PartStatus::PENDING.can_transition_to(PartStatus::OUTSOURCE));
    }

    #[test]
    fn allowed_phase1_scan_inspect_fail() {
        // INSPECTION → REPAIRING（scan-inspect FAIL：品检打回返修）
        assert!(PartStatus::INSPECTION.can_transition_to(PartStatus::REPAIRING));
    }

    #[test]
    fn allowed_phase1_cancel_repairing_outsource() {
        // REPAIRING / OUTSOURCE 也允许 → CANCELLED
        assert!(PartStatus::REPAIRING.can_transition_to(PartStatus::CANCELLED));
        assert!(PartStatus::OUTSOURCE.can_transition_to(PartStatus::CANCELLED));
    }

    #[test]
    fn disallowed_phase1_self_loops_and_invalid() {
        // 自环非法（所有状态）
        for s in [
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
        ] {
            assert!(!s.can_transition_to(s), "{s:?} 自环必须拒绝");
        }
        // COMPLETED → 任何状态都非法
        for t in [
            PartStatus::PENDING,
            PartStatus::PROGRAMMING,
            PartStatus::IN_PROCESS,
            PartStatus::INSPECTION,
            PartStatus::READY_TO_SHIP,
            PartStatus::DELIVERED,
            PartStatus::REPAIRING,
            PartStatus::OUTSOURCE,
            PartStatus::CANCELLED,
        ] {
            assert!(
                !PartStatus::COMPLETED.can_transition_to(t),
                "COMPLETED → {t:?} 必须拒绝"
            );
        }
        // CANCELLED → 任何状态都非法
        for t in [
            PartStatus::PENDING,
            PartStatus::PROGRAMMING,
            PartStatus::IN_PROCESS,
            PartStatus::INSPECTION,
            PartStatus::READY_TO_SHIP,
            PartStatus::DELIVERED,
            PartStatus::REPAIRING,
            PartStatus::OUTSOURCE,
            PartStatus::COMPLETED,
        ] {
            assert!(
                !PartStatus::CANCELLED.can_transition_to(t),
                "CANCELLED → {t:?} 必须拒绝"
            );
        }
        // OUTSOURCE → DELIVERED 非法（必须先经 READY_TO_SHIP）
        assert!(!PartStatus::OUTSOURCE.can_transition_to(PartStatus::DELIVERED));
        // REPAIRING → READY_TO_SHIP 非法（必须先经 INSPECTION → READY_TO_SHIP）
        assert!(!PartStatus::REPAIRING.can_transition_to(PartStatus::READY_TO_SHIP));
        // PROGRAMMING → OUTSOURCE 非法（必须先经 IN_PROCESS）
        assert!(!PartStatus::PROGRAMMING.can_transition_to(PartStatus::OUTSOURCE));
        // INSPECTION → OUTSOURCE 非法（不能跳过 IN_PROCESS）
        assert!(!PartStatus::INSPECTION.can_transition_to(PartStatus::OUTSOURCE));
        // DELIVERED → INSPECTION 非法（不可回退）
        assert!(!PartStatus::DELIVERED.can_transition_to(PartStatus::INSPECTION));
        // DELIVERED → REPAIRING 非法（不允许从 DELIVERED 直接进 REPAIRING）
        assert!(!PartStatus::DELIVERED.can_transition_to(PartStatus::REPAIRING));
        // READY_TO_SHIP → REPAIRING 非法
        assert!(!PartStatus::READY_TO_SHIP.can_transition_to(PartStatus::REPAIRING));
        // OUTSOURCE → REPAIRING 非法（必须先经 IN_PROCESS）
        assert!(!PartStatus::OUTSOURCE.can_transition_to(PartStatus::REPAIRING));
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
///
/// 2026-09-16 PR-2 瘦身（migration 027）：`location` / `current_holder_id` /
/// `placed_at` 列已从 t_part 删除，rollup 投影同步收窄。`next_process_id` 仍
/// 保留作 part 派生列读缓存（service 层按 `next_process_id` 决策下一步）。
///
/// 2026-09-16 PR-3 批次 step 化（migration 028）：
/// - 删 `placed_at`（t_part_batch 列已删）
/// - `next_process_id` → `current_process_step_id`（t_part_batch 新列，逻辑 FK → step.id）
///
/// 2026-09-30 改直读 `current_process_id`（migration 004，逻辑 FK → t_process.id）：
/// 字段由 `current_process_step_id` 改名。原先 rollup 派生只拿 step_id，再由
/// service 层**额外发一次 SELECT** 去 `t_process_chain_step` 翻 `process_id`；
/// 且「最慢批次」（min progress）只按 status 挑、不看工序，若它 step_id 为 NULL
/// （无工序链工单的常态）就会把整个工单的 `t_part.next_process_id` 抹成 NULL，
/// 而该列是删工序的保护条件之一（`prod/process` 的 `count_referencing` 5 个
/// 子查询之一）。新列 `current_process_id` 是批次工序池归属的**权威依据**，
/// 直读它即可，无需 step 中转。
#[derive(Debug, Clone, Default)]
pub struct BatchForRollup {
    pub status: String,
    pub location: Option<String>,
    pub current_holder_id: Option<i64>,
    /// 逻辑 FK → `t_process.id`；2026-09-30 改直读新列（migration 004）。
    pub current_process_id: Option<i64>,
}

/// `compute_part_target` 的输出：目标 status + 派生列（从最慢批次物化）。
///
/// 2026-09-16 PR-2 瘦身：只物化 `status` + `next_process_id`（t_part 保留的两
/// 列 rollup 缓存）。`location` / `current_holder_id` / `placed_at` 真相源在
/// `t_part_batch`，由 `sync_from_batch_change` 的 caller 在需要时另查。
///
/// 2026-09-16 PR-3 批次 step 化：`next_process_id` 字段保留作为派生缓存值，
/// 但派生源改为 `BatchForRollup.current_process_step_id`（service 层
/// `sync_from_batch_change` JOIN `t_process_chain_step` 取 `process_id` 写入）。
/// 本函数只搬运 step_id → next_process_id（语义对齐：step 进程维度 1:1，
/// rollup 派生保留同一 process_id）。
///
/// 2026-09-30 改直读 `current_process_id`（migration 004）：字段名同步改为
/// `current_process_id`，且**这次名实相符**——承载的确实是 `t_process.id`
/// （原 `next_process_id` 名下装的是 step_id，误导性命名）。DB 列名与对外
/// DTO 字段名 `t_part.next_process_id` 不动，仅内部 struct 改名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRollupTarget {
    pub status: String,
    /// 2026-09-30：由「实装 step_id 的 next_process_id」改为真正的 process_id。
    pub current_process_id: Option<i64>,
}

/// batch 集 → part rollup target（part/assembly/batch 重构方案 §4.2 步骤 1–6）。
///
/// 规则（与 assembly 端 `compute_assembly_target` 同构，但 part 与 batch 状态
/// 词汇相同，直接取 status 字符串）：
///
/// 1. 空集 → `None`（防御；caller 走 NoChange）
/// 2. 全部 CANCELLED → `status='CANCELLED'`
/// 3. 非 CANCELLED 全部 COMPLETED → `status='COMPLETED'`
/// 4. 否则取「最慢批次」（min progress over non-terminal, non-cancelled）的
///    `status` + `current_process_id`（`t_part.next_process_id` 派生缓存）；
///    progress 表见 `part_status_progress`
///
/// 2026-09-16 PR-2 瘦身：返回值只剩 `status` + `next_process_id`；其它派
/// 生列（`location` / `current_holder_id` / `placed_at`）真相源在
/// `t_part_batch`，由 `sync_from_batch_change` 的 caller 在需要时另查。
///
/// 2026-09-30 改直读 `current_process_id`（migration 004）：本函数保持纯函数
/// 性质（不引入 SQL、不查 `t_process_chain_step`）。caller
/// `sync_from_batch_change` 直接把 `current_process_id` 写进
/// `t_part.next_process_id`，省掉原先「step_id → 一次 SELECT → process_id」的
/// 转译往返。
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
            current_process_id: r.current_process_id,
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
            current_process_id: r.current_process_id,
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
        current_process_id: min.current_process_id,
    })
}

#[cfg(test)]
mod rollup_tests {
    use super::*;

    fn batch(status: &str) -> BatchForRollup {
        BatchForRollup {
            status: status.to_string(),
            location: None,
            current_holder_id: None,
            current_process_id: None,
        }
    }

    fn batch_with_loc(status: &str, loc: Option<&str>) -> BatchForRollup {
        BatchForRollup {
            status: status.to_string(),
            location: loc.map(str::to_string),
            current_holder_id: None,
            current_process_id: None,
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
    fn materializes_next_process_from_min_progress_batch() {
        // 2026-09-16 PR-2 瘦身：rollup 只物化 status + next_process_id。
        // 验证 next_process_id 从 min-progress 批次派生（其它派生列忽略）。
        //
        // 2026-09-30 改直读 current_process_id（migration 004）：本函数纯搬运
        // t_part_batch.current_process_id（真 process_id），caller 直接写
        // t_part.next_process_id，不再经 t_process_chain_step 中转。
        let mut b_in_process = batch_with_loc("IN_PROCESS", Some("PRODUCTION_SHELF"));
        b_in_process.current_holder_id = Some(42);
        b_in_process.current_process_id = Some(7);
        let v = vec![
            b_in_process,
            batch_with_loc("DELIVERED", Some("OUTSOURCE_COMPANY")),
        ];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "IN_PROCESS");
        assert_eq!(r.current_process_id, Some(7));
    }

    #[test]
    fn all_cancelled_returns_first_batch_next_process() {
        let v = vec![
            batch_with_loc("CANCELLED", Some("OFFICE")),
            batch_with_loc("CANCELLED", Some("PRODUCTION_SHELF")),
        ];
        let r = compute_part_target(&v).unwrap();
        assert_eq!(r.status, "CANCELLED");
        // current_process_id 默认 None，无 active 派生
        assert_eq!(r.current_process_id, None);
    }
}
