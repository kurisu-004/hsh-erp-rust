//! delivery_note 域 P3 attach-batches 端点响应 VO

use serde::Serialize;

/// `POST /api/v2/delivery-notes/{note_id}/attach-batches` 响应。
///
/// 即使部分失败也始终返回 200，前端按 `conflicts` 列表做差异处理：
/// - 全失败：`attached=0`、`conflicts` 非空
/// - 部分失败：`attached>0`、`conflicts` 列出失败项
/// - 全部成功：`attached=n`、`conflicts=[]`
///
/// `note_id` 本身非 DRAFT（409）属于硬错误，不入本结构；OCC / 状态非法 /
/// 重复 attach / 批次不存在 / 跨单等均在 `conflicts[].reason` 中以字符串表达。
#[derive(Debug, Clone, Serialize)]
pub struct AttachBatchesOut {
    pub attached: usize,
    pub conflicts: Vec<AttachBatchConflict>,
}

/// 单个失败项（attach_batches 响应）。
///
/// `reason` 是稳定的 SCREAMING_SNAKE_CASE 字符串，便于前端 i18n / 分类：
/// - `BATCH_NOT_FOUND` — 批次 id 不存在 / 已软删
/// - `ALREADY_ATTACHED` — `delivery_note_id IS NOT NULL`（已挂在某张单上）
/// - `INVALID_STATE:<STATUS>` — 批次当前 status 不在 A 组
///   （`INSPECTION` / `READY_TO_SHIP`）；尖括号内为原 status 值
/// - `VERSION_CONFLICT` — item.version 与 DB 不一致（OCC 失败）
#[derive(Debug, Clone, Serialize)]
pub struct AttachBatchConflict {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub reason: String,
}