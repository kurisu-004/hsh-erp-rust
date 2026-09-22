//! `scan_add` DTO 投影小 helpers + `NoteScope::classify` impl（2026-09-22 D-5 拆出）
//!
//! ⚠️ **禁止合并为 generic helper**：`AvailableBatchDto` / `AttachableBatchDto`
//! 当前字段同形，但设计上独立——未来字段分叉（status 派生逻辑、OCC version
//! 来源、扩展字段）时各自演化。强行复用 generic 会导致所有调用点耦合。

use crate::modules::delivery_note::dto::{
    AttachableBatchDto, AvailableBatchDto, BatchStatusDto,
};
use crate::modules::delivery_note::model::NoteScope;
use crate::modules::part::batch::model::TPartBatch;

use super::super::inner::GroupWithMemberIds;

/// `TPartBatch` → `AvailableBatchDto`（B 组）。
///
/// 状态解析失败兜底为 `Pending`（与原 `build_unresolved_target` 行为一致）。
pub(super) fn to_available_batch_dto(b: TPartBatch) -> AvailableBatchDto {
    AvailableBatchDto {
        batch_id: b.id,
        version: b.version,
        quantity: b.quantity,
        status: BatchStatusDto::from_db(&b.status).unwrap_or(BatchStatusDto::Pending),
    }
}

/// `TPartBatch` → `AttachableBatchDto`（A 组；status 仅有 INSPECTION / READY_TO_SHIP）。
///
/// 状态解析失败兜底为 `Pending`（与原 `build_unresolved_target` 行为一致）。
pub(super) fn to_attachable_batch_dto(b: TPartBatch) -> AttachableBatchDto {
    AttachableBatchDto {
        batch_id: b.id,
        version: b.version,
        quantity: b.quantity,
        status: BatchStatusDto::from_db(&b.status).unwrap_or(BatchStatusDto::Pending),
    }
}

/// `NoteScope::classify`（设计 §3.2）：根据锚点 leaf 客户 + L1 全部分组
/// → 投影为 `L1Wide` / `Group(gid)` / `Leaf(cid)` 三态之一。
///
/// - 空分组 → L1Wide（兜底）
/// - 任一分组含 leaf → Group(gid)
/// - 其它 → Leaf(cid)（锚点单厂单）
impl NoteScope {
    pub(super) fn classify(leaf_customer_id: i64, groups: &[GroupWithMemberIds]) -> Self {
        if groups.is_empty() {
            return Self::L1Wide;
        }
        for g in groups {
            if g.member_ids.contains(&leaf_customer_id) {
                return Self::Group(g.group_id);
            }
        }
        Self::Leaf(leaf_customer_id)
    }
}