//! `scan_add` DTO 投影小 helpers
//!
//! ⚠️ **禁止合并为 generic helper**：`AvailableBatchDto` / `AttachableBatchDto`
//! 当前字段同形，但设计上独立——未来字段分叉（status 派生逻辑、OCC version
//! 来源、扩展字段）时各自演化。强行复用 generic 会导致所有调用点耦合。
//!
//! 2026-10-08：`NoteScope::classify` impl 随范围判定下线删除。

use crate::modules::com::delivery_note::vo::{
    AttachableBatchDto, AvailableBatchDto, BatchStatusDto,
};
use crate::shared::batch::TPartBatch;

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
