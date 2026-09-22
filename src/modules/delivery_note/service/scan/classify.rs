//! `scan_add` 5 组分类 helpers（2026-09-22 D-5 拆出）
//!
//! 单元测试保留在 `tests.rs`（review 第 1 轮抽出，本文件只留生产 helpers）。

use std::collections::HashMap;

use crate::modules::delivery_note::dto::{ScanOutcomeDto, UnresolvedTargetDto};
use crate::modules::part::model::TPart;
use crate::modules::part::batch::model::TPartBatch;

use super::helpers;

// ---------------------------------------------------------------------------
//  batch 状态 5 类分组（设计：scan-route-b-fix.md）
// ---------------------------------------------------------------------------

/// A 组：可直接 attach 入单（INSPECTION + READY_TO_SHIP）。
///
/// `pub(crate)`：service::scan 与 service::attach 共用一份定义；不要在
/// service/ 之外的代码里直接调用，attach 模块走 `super::scan::is_attachable_state`。
pub(crate) fn is_attachable_state(status: &str) -> bool {
    matches!(status, "READY_TO_SHIP" | "INSPECTION")
}

/// B 组：可送检。`IN_PROCESS` 需未被工人持有。
///
/// 「工人持有」以 `location = 'WORKER'` 判定（与 worker_pool / part repo 的
/// 全部查询一致）。**不能用 `current_holder_id`**：该列多态——批次放货架时
/// 存 `t_shelf.id`（`location = 'PRODUCTION_SHELF' / 'INSPECTION_SHELF'`），
/// 只有工人取件时才存 worker id（`location = 'WORKER'`）。
pub(super) fn is_inspectable_state(b: &TPartBatch) -> bool {
    match b.status.as_str() {
        "PENDING" | "PROGRAMMING" | "REPAIRING" => true,
        "IN_PROCESS" => b.location.as_deref() != Some("WORKER"),
        _ => false,
    }
}

/// C 组：直接报错的非法状态。`IN_PROCESS` 被工人持有（`location = 'WORKER'`）归此类。
pub(crate) fn classify_invalid_state(b: &TPartBatch) -> Option<&'static str> {
    match b.status.as_str() {
        "DELIVERED" => Some("DELIVERED"),
        "OUTSOURCE" => Some("OUTSOURCE"),
        "COMPLETED" => Some("COMPLETED"),
        "CANCELLED" => Some("CANCELLED"),
        "IN_PROCESS" if b.location.as_deref() == Some("WORKER") => {
            Some("IN_PROCESS_HELD_BY_WORKER")
        }
        _ => None,
    }
}

/// 单 target（part）的 batch 4 类分组结果（A/B/D）。
///
/// A 组：attachable（INSPECTION + READY_TO_SHIP）
/// B 组：inspectable（PENDING/PROGRAMMING/REPAIRING/IN_PROCESS 非工人持有）
/// D 组：conflict（已挂别的 active 单，由 service 层后续判定 21406）
///
/// C 组（DELIVERED/OUTSOURCE/COMPLETED/CANCELLED/IN_PROCESS 工人持有）由前置
/// `has_fully_invalid_target` 静默过滤，不入此 struct。
///
/// `had_invalid` 记录该 target 在 C 组过滤前**是否至少有 1 个 C 组 batch**。
/// 即便过滤后只剩 A/B，该 target 也会强制走弹窗路径（`classify_outcome` 短路），
/// 让前端能看到剩余的合法批次让用户确认（spec：前端必须能看到 C 被过滤的迹象，
/// 用户的语义预期是「即使只看到 A，也应该先确认再 attach」）。
///
/// `delivery_note_id == Some(note.id)` 的 batch 视为「已挂本单」不入任何 Vec，
/// 由调用方按需要去重。
pub(super) struct TargetEvaluation {
    pub(super) part: TPart,
    pub(super) attachable: Vec<TPartBatch>,
    pub(super) inspectable: Vec<TPartBatch>,
    pub(super) conflict: Vec<TPartBatch>,
    /// 该 target 在 5 组分类前是否有 C 组被静默过滤。
    /// 即使分类后只剩 A，也会强制走弹窗路径（不让 A 静默自动 attach）。
    pub(super) had_invalid: bool,
}

/// 5 组分类的 outcome 判定（纯函数，单测覆盖）。
///
/// 输入是从 evaluations 聚合而来的四个布尔量；返回的 ScanOutcomeDto 决定
/// handler 层后续是否 attach，以及响应里 `unresolved_targets` 的形状。
///
/// 关键约束：只要任一 target 原始有 C 组被过滤（C@WORKER / DELIVERED /
/// OUTSOURCE / COMPLETED / CANCELLED），就强制走弹窗路径
/// （CandidatesAvailable / PartialAdded），即使用户最终看不到 C 也要走弹窗。
/// 这是 spec 约定：前端代码依赖 `unresolved_targets` 展示剩余合法批次让
/// 用户确认，不能让 A 在 C 被静默过滤的语义下静默自动 attach。
pub(super) fn classify_outcome(
    is_assembly: bool,
    any_inspectable: bool,
    all_attachable_empty: bool,
    any_had_invalid_filtered: bool,
) -> ScanOutcomeDto {
    if any_had_invalid_filtered {
        return if is_assembly {
            ScanOutcomeDto::PartialAdded
        } else {
            ScanOutcomeDto::CandidatesAvailable
        };
    }
    match (is_assembly, any_inspectable) {
        (false, true) => ScanOutcomeDto::CandidatesAvailable,
        (true, true) => ScanOutcomeDto::PartialAdded,
        (_, false) => {
            if all_attachable_empty {
                ScanOutcomeDto::AlreadyPresent
            } else {
                ScanOutcomeDto::Added
            }
        }
    }
}

/// 全 conflict 短路判定（保留 21406 硬错误；纯函数）。
///
/// 每个 target 的 conflict 非空、attachable 与 inspectable 都为空 → 用户
/// 期望的批次全被别的 active 单锁死。返回 true 时 caller 应直接
/// `BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED` 报错。
pub(super) fn is_all_conflict(evaluations: &[TargetEvaluation]) -> bool {
    evaluations
        .iter()
        .all(|e| e.attachable.is_empty() && e.inspectable.is_empty() && !e.conflict.is_empty())
}

/// C 组分布判定（保留 21421 硬错误；纯函数，单测覆盖）。
///
/// 替代原「任一 C → 21421」全-or-无短路：原本工人持有（C）与货架上
/// （A/B）的合法批次同存于一个子零件时，会错误地整单拒绝。改成
/// 「按 part_id 聚合 → 任一 target 全 C 才报错」，且 C 组静默过滤，
/// 让前端弹窗只看到合法 B 组候选。
///
/// 返回 true 当且仅当存在至少一个 `part_id`，其加载到的全部 batch
/// 都落在 `classify_invalid_state` 命中集里。
pub(super) fn has_fully_invalid_target(batches: &[TPartBatch]) -> bool {
    let mut by_part_total: HashMap<i64, usize> = HashMap::new();
    let mut by_part_invalid: HashMap<i64, usize> = HashMap::new();
    for b in batches {
        *by_part_total.entry(b.part_id).or_insert(0) += 1;
        if classify_invalid_state(b).is_some() {
            *by_part_invalid.entry(b.part_id).or_insert(0) += 1;
        }
    }
    by_part_total.iter().any(|(part_id, total)| {
        *total > 0 && by_part_invalid.get(part_id).copied().unwrap_or(0) == *total
    })
}

/// 由 evaluations[i] 构造 `UnresolvedTargetDto`（含 part 元数据 + A/B 组批次）。
pub(super) fn build_unresolved_target(e: TargetEvaluation) -> UnresolvedTargetDto {
    UnresolvedTargetDto {
        part_id: e.part.id,
        serial_no: e.part.serial_no.clone().unwrap_or_default(),
        drawing_no: e.part.drawing_no.clone(),
        name: e.part.name.clone(),
        available_batches: e
            .inspectable
            .into_iter()
            .map(helpers::to_available_batch_dto)
            .collect(),
        attachable_batches: e
            .attachable
            .into_iter()
            .map(helpers::to_attachable_batch_dto)
            .collect(),
    }
}