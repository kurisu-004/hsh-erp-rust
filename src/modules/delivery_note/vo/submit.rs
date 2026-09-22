//! delivery_note 域 P2 submit 端点响应 VO

use serde::Serialize;

use super::delivery_note::DeliveryNoteOut;
use super::scan::UnresolvedTargetDto;

/// `POST /delivery-notes/{id}/submit` 的结果类别。
///
/// - `SUBMITTED`：全部批次已 `READY_TO_SHIP`，状态机 DRAFT → SUBMITTED 已提交；
///   `note` 字段返回提交后的送货单投影。
/// - `CANDIDATES_AVAILABLE`：存在仍在 `INSPECTION` 的批次，**本次未提交**；
///   返回这些批次供前端一键过检（转发 `POST /parts/batch-to-ship`）；
///   `note` 为 `null`（未提交，无新状态可返回）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubmitOutcomeDto {
    Submitted,
    CandidatesAvailable,
}

/// `POST /delivery-notes/{id}/submit` 出参（200 OK）。
///
/// `outcome = SUBMITTED` → `note` 为提交后的送货单投影，`unresolved_targets` 缺省不序列化。
/// `outcome = CANDIDATES_AVAILABLE` → `note` 为 `null`（未提交，无新状态可返回），
///   `unresolved_targets` 按 part 分组列出仍在 `INSPECTION` 的已挂单批次。
///
/// `note` 字段故意不挂 `skip_serializing_if`：候选分支必须以字面量 `null`
/// 显式出现，让前端判 `note != null` 即可识别提交是否真的发生过。
/// `unresolved_targets` 反过来必须挂：`SUBMITTED` 分支不出现该字段，避免泄漏 INSPECTION 详情。
#[derive(Debug, Clone, Serialize)]
pub struct SubmitDeliveryOut {
    pub outcome: SubmitOutcomeDto,
    pub note: Option<DeliveryNoteOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved_targets: Option<Vec<UnresolvedTargetDto>>,
}