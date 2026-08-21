//! delivery_note 域 DTO
//!
//! 对应 Python myERP/schema/delivery_note.py。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxOut`：单条详情出参（id 字段用 `#[serde(serialize_with = "shared::types::serialize_i64")]`）
//! - `XxxListItem` / `XxxListOut`：列表分页
//!
//! ## Phase 范围
//! - **P1**：送货分组（§6.1）
//! - **P2**：送货单生命周期 + 候选入单（不含扫码 P3 / 打印 P4）

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
//  出参
// ---------------------------------------------------------------------------

/// 组成员出参（id 序列化为字符串，避免 JS 精度截断）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupMemberOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub customer_name: String,
}

/// 分组头出参
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub name: String,
    pub members: Vec<DeliveryGroupMemberOut>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 组外 L2 出参（按设计 §6.1：所有未入组的 L2）
#[derive(Debug, Clone, Serialize)]
pub struct UngroupedCustomerOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
}

/// 分组列表出参（GET /delivery-groups）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupListOut {
    pub groups: Vec<DeliveryGroupOut>,
    pub ungrouped_customers: Vec<UngroupedCustomerOut>,
}

// ---------------------------------------------------------------------------
//  入参
// ---------------------------------------------------------------------------

/// 创建分组入参（POST /delivery-groups）
///
/// `member_customer_ids` 是**初始成员集合**，新增分组时一次性写入。
/// `name` 长度 1..=100（与 DB 列 `varchar(100)` 对齐），空白字符串 trim 后为空则拒。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateDeliveryGroupRequest {
    /// L1 客户的雪花 id（请求 JSON 字符串）
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub customer_id: i64,
    /// 分组名（trim 后 1..=100）
    pub name: String,
    /// 成员 L2 客户 id 列表（字符串形式；空 Vec 表示创建时无成员）
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64_vec")]
    pub member_customer_ids: Vec<i64>,
}

/// 更新分组入参（POST /delivery-groups/{id}/update）
///
/// 字段语义：
/// - `version`：必填，用于乐观锁（req 与 DB 当前 version 不一致 → 409 / VERSION_CONFLICT）
/// - `name`：None = 不改；Some(trim 后空) = 400；Some(>100 字符) = 400
/// - `member_customer_ids`：None = 不改；Some(vec) = **全量替换**
///   （缺失成员软删、新增成员插入；同 tx 内校验成员冲突 21415）
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateDeliveryGroupRequest {
    pub version: i32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_i64_vec_opt")]
    pub member_customer_ids: Option<Vec<i64>>,
}

/// 软删除分组入参（POST /delivery-groups/{id}/soft-delete）
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryGroupIdRequest {
    pub version: i32,
}

// ===========================================================================
//  P2：送货单生命周期 DTO（移植 + 范围字段扩展）
// ===========================================================================

// ---------------------------------------------------------------------------
//  出参：单子概要 / 详情 / 行项
// ---------------------------------------------------------------------------

/// 送货单概要（list + 大部分接口的公共响应）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub delivery_note_no: String,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub customer_path: Option<String>,
    pub status: String,
    pub submitted_at: Option<NaiveDateTime>,
    pub picked_up_at: Option<NaiveDateTime>,
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt", skip_serializing_if = "Option::is_none")]
    pub submitted_by: Option<i64>,
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt", skip_serializing_if = "Option::is_none")]
    pub picked_up_by: Option<i64>,
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt", skip_serializing_if = "Option::is_none")]
    pub driver_worker_id: Option<i64>,
    pub driver_worker_name: Option<String>,
    pub part_count: i64,
    pub note: Option<String>,
    pub delivery_date: Option<NaiveDate>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    /// 范围字段（D1 范围列）
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt", skip_serializing_if = "Option::is_none")]
    pub delivery_group_id: Option<i64>,
    pub delivery_group_name: Option<String>,
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt", skip_serializing_if = "Option::is_none")]
    pub leaf_customer_id: Option<i64>,
    pub leaf_customer_name: Option<String>,
    /// 范围展示文案（设计 §6.2：分组名 / L2 名 / L1 名）
    pub scope_label: Option<String>,
}

/// 送货单下一行零件的投影（行=批次；id = batch_id）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteLineItem {
    /// 批次 id（行身份）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    /// 工单 id
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub batch_label: String,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub status: String,
    pub applicant_name: Option<String>,
    pub request_date: Option<NaiveDate>,
    pub planned_delivery_date: Option<NaiveDate>,
    pub system_delivery_date: Option<NaiveDate>,
    pub order_no: Option<String>,
    pub note: Option<String>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub customer_path: Option<String>,
    /// 兼容字段（前端两种命名都接受）
    pub is_scanned: bool,
    pub scanned: bool,
    /// 装配件父行字段（仅子件行填；散件 None）
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt", skip_serializing_if = "Option::is_none")]
    pub assembly_id: Option<i64>,
    pub assembly_serial_no: Option<String>,
    pub assembly_drawing_no: Option<String>,
    pub assembly_name: Option<String>,
    pub assembly_order_no: Option<String>,
}

/// 送货单详情（head + line_items + 扫码进度）。
///
/// `scanned_serials` 在 P2 阶段始终为空数组（Python 2026-07-23 起后端不再维护
/// 扫码状态，由前端本地 Set 驱动）；保留字段以保持 schema 兼容。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteDetailOut {
    #[serde(flatten)]
    pub head: DeliveryNoteOut,
    pub line_items: Vec<DeliveryNoteLineItem>,
    pub scanned_serials: Vec<String>,
}

/// 送货单事件条目（时间线）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteEventOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub delivery_note_id: i64,
    pub event_type: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub note: Option<String>,
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt", skip_serializing_if = "Option::is_none")]
    pub created_by: Option<i64>,
    pub created_at: Option<NaiveDateTime>,
}

// ---------------------------------------------------------------------------
//  入参
// ---------------------------------------------------------------------------

/// 入单条目（批次 + 可选部分数量）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteAddItem {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub batch_id: i64,
    /// None = 整批；Some(n) 且 n < batch.quantity → 服务端拆分
    pub quantity: Option<i32>,
}

/// 创建草稿入参（POST /delivery-notes）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteCreateRequest {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub customer_id: i64,
    pub delivery_date: Option<NaiveDate>,
    #[serde(default)]
    pub items: Vec<DeliveryNoteAddItem>,
    pub note: Option<String>,
}

/// 添加零件入参（POST /delivery-notes/{id}/add-parts）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteAddPartsRequest {
    pub items: Vec<DeliveryNoteAddItem>,
    pub version: i32,
}

/// 移除零件入参（POST /delivery-notes/{id}/remove-parts）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteRemovePartsRequest {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64_vec")]
    pub batch_ids: Vec<i64>,
    pub version: i32,
}

/// 通用 version OCC 入参（submit / recall / pickup / soft-delete）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteVersionedRequest {
    pub version: i32,
}

/// partial update 入参（POST /delivery-notes/{id}/update；DRAFT/SUBMITTED）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteUpdateRequest {
    pub version: i32,
    pub delivery_date: Option<NaiveDate>,
    pub note: Option<String>,
}

/// 扫码入单（每扫一个件一次；P3 主用，P2 保留 stub 兼容性）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePickupScanRequest {
    pub part_serial: String,
    pub badge_code: Option<String>,
}

/// 扫码响应（P2 始终 `scanned_count=0 / scanned_serials=[] / ready=false`，
/// 与 Python 2026-07-23 起后端行为一致）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNotePickupScanOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub delivery_note_id: i64,
    pub scanned_count: i64,
    pub expected_count: i64,
    pub ready: bool,
    pub scanned_serials: Vec<String>,
}

/// 领取入参（POST /delivery-notes/{id}/pickup）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePickupRequest {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub driver_worker_id: i64,
    pub badge_code: Option<String>,
    pub version: i32,
}

// ---------------------------------------------------------------------------
//  列表响应
// ---------------------------------------------------------------------------

/// 一览响应（GET /delivery-notes；含分页总计）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteListOut {
    pub items: Vec<DeliveryNoteOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 待司机领取一览（GET /delivery-notes/pickup-pending）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNotePickupListOut {
    pub items: Vec<DeliveryNoteOut>,
}

// ---------------------------------------------------------------------------
//  候选入单（GET /delivery-notes/candidate-parts）
// ---------------------------------------------------------------------------

/// 候选入单零件（INSPECTION + READY_TO_SHIP 批次，同 L1 根，不在 active 单上）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteCandidatePart {
    /// 工单 id（展示 / 反查用）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    /// 批次 id（入单回传用）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    pub batch_label: String,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
    pub quantity: i32,
    pub applicant_name: Option<String>,
    pub status: String,
    pub planned_delivery_date: Option<NaiveDate>,
    pub order_no: Option<String>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub customer_path: Option<String>,
}

/// 候选入单响应（GET /delivery-notes/candidate-parts）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteCandidatePartsOut {
    pub items: Vec<DeliveryNoteCandidatePart>,
}

// ---------------------------------------------------------------------------
//  列表 query DTO
// ---------------------------------------------------------------------------

/// GET /delivery-notes 查询参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteListQuery {
    pub statuses: Option<Vec<String>>,
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_i64_opt")]
    pub customer_id: Option<i64>,
    pub keyword: Option<String>,
    pub sort_by: Option<String>,
    pub sort_dir: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// GET /delivery-notes/pickup-pending 查询参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePickupPendingQuery {
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_i64_opt")]
    pub customer_id: Option<i64>,
}

/// GET /delivery-notes/candidate-parts 查询参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteCandidatePartsQuery {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub customer_id: i64,
}

/// GET /delivery-notes/{id} 路径参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePath {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub id: i64,
}