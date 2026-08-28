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
//! - **P3**：扫码入单（§5）—— Scan* DTO

use std::collections::HashMap;

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

/// `GET /delivery-notes/batch-detail?ids=...` 响应载体。
///
/// 仅作为 `items: [DeliveryNoteDetailOut]` 的轻量封装，避免 schema 顶层直接
/// 给出数组（信封 `data` 不能是裸数组）。`DeliveryNoteDetailOut` 自身已
/// `#[serde(flatten)] head: DeliveryNoteOut`，因此每个 item 在 wire 上仍是
/// head + `line_items` + `scanned_serials` 的扁平结构。
#[derive(Debug, Clone, Serialize)]
pub struct BatchDeliveryDetailData {
    pub items: Vec<DeliveryNoteDetailOut>,
}

/// `GET /delivery-notes/batch-detail?ids=1,2,3` 查询参数。
/// `ids` 为可选；handler 内部做 split/trim/filter/dedupe/parse i64 + 1..=200
/// 校验。这里只声明 query 形状。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteBatchDetailQuery {
    pub ids: Option<String>,
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
///
/// `statuses` 是逗号分隔字符串（axum 默认 Query 不支持重复 key）：
/// `?statuses=DRAFT,SUBMITTED`。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteListQuery {
    pub statuses: Option<String>,
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

// ===========================================================================
//  P4：打印入参 DTO（设计 §8，POST /delivery-notes/{id}/print[/-labels]）
// ===========================================================================

/// 送货单打印入参（POST /delivery-notes/{id}/print）。
///
/// - `custom_order`: 代表 batch id 序列；与 `note.line_items[*].id` 一一对应；
///   非法（含不在本单 id / 漏行）→ 422 `BIZ_DELIVERY_PRINT_BAD_ORDER`。
/// - `merge_assemblies`: true → 同装配件子件合并一行（默认 false，沿用 Python
///   送货单默认；labels 路径强制 true）。
/// - `merge_quantities`: 按装配件 id 覆盖合并行数量（默认 1 套）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PrintDeliveryNoteRequest {
    #[serde(default)]
    pub custom_order: Option<Vec<String>>,
    #[serde(default)]
    pub merge_assemblies: Option<bool>,
    #[serde(default)]
    pub merge_quantities: Option<HashMap<String, i32>>,
}

/// 标签打印入参（POST /delivery-notes/{id}/print-labels）。
///
/// 字段语义同 [`PrintDeliveryNoteRequest`]，增 `line_item_ids`：
/// - `None` / 缺省 → 全部数据行
/// - `Some([])` → 400 `BIZ_INVALID_VALUE`
/// - 未知 batch id → 422 `BIZ_DELIVERY_PRINT_BAD_ORDER`
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PrintLabelsRequest {
    #[serde(default)]
    pub custom_order: Option<Vec<String>>,
    #[serde(default)]
    pub merge_assemblies: Option<bool>,
    #[serde(default)]
    pub merge_quantities: Option<HashMap<String, i32>>,
    #[serde(default)]
    pub line_item_ids: Option<Vec<String>>,
}

// ===========================================================================
//  P3：扫码入单 DTO（设计 §5，POST /delivery-notes/scan）
// ===========================================================================

/// 扫码入单请求体。
///
/// `code` 是 trim 后的扫码载荷，长度要求 1..=64 字符；空白 / 空 → 400
/// `BIZ_INVALID_VALUE`。
#[derive(Debug, Clone, Deserialize)]
pub struct ScanDeliveryRequest {
    pub code: String,
}

/// 扫码入单结果（200 OK 路径）。
///
/// - `ADDED`：A 组覆盖所有 target，本次成功挂载 ≥1 个
/// - `ALREADY_PRESENT`：A 组覆盖所有 target，但都已在本单（幂等）
/// - `CANDIDATES_AVAILABLE`：散件仅 B 组 → unresolved_targets 单元素
/// - `PARTIAL_ADDED`：装配件 A+B 混合 → unresolved_targets 多元素
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScanOutcomeDto {
    Added,
    AlreadyPresent,
    CandidatesAvailable,
    PartialAdded,
}

/// `t_part_batch.status` 强类型投影。序列化沿用 DB 列值（SCREAMING_SNAKE_CASE）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BatchStatusDto {
    Pending,
    Programming,
    InProcess,
    Inspection,
    ReadyToShip,
    Delivered,
    Repairing,
    Outsource,
    Completed,
    Cancelled,
}

impl BatchStatusDto {
    /// 由 DB 字符串反序列化为枚举；未知值返回 `None`。
    #[allow(clippy::should_implement_trait)]
    pub fn from_db(s: &str) -> Option<Self> {
        Some(match s {
            "PENDING" => Self::Pending,
            "PROGRAMMING" => Self::Programming,
            "IN_PROCESS" => Self::InProcess,
            "INSPECTION" => Self::Inspection,
            "READY_TO_SHIP" => Self::ReadyToShip,
            "DELIVERED" => Self::Delivered,
            "REPAIRING" => Self::Repairing,
            "OUTSOURCE" => Self::Outsource,
            "COMPLETED" => Self::Completed,
            "CANCELLED" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// 解析结果类别（驱动前端"是装配件还是散件"决策）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResolvedKindDto {
    Part,
    Assembly,
}

/// 解析结果（识别出来的实体）。
///
/// - `kind = Part`：单工单（可能隶属于某个装配件的子件）；`id` = part.id
/// - `kind = Assembly`：扫的是装配件总图，`id` = assembly.id
///
/// **路线 B 重构（2026-08-27）移除字段**：
/// - `assembly_id`：scan 路径不会扫到子件；如需查父装配体走 `GET /api/v2/assemblies/{id}`
/// - `child_count`：候选列表响应里前端不需要"装配体有几个子件"；要查子件清单走 `GET /api/v2/assemblies/{id}/children`
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedEntityDto {
    pub kind: ResolvedKindDto,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
}

/// 扫码命中的送货单概要（响应里的 `note` 字段）。
#[derive(Debug, Clone, Serialize)]
pub struct ScanDeliveryNoteSummaryDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub delivery_note_no: String,
    pub version: i32,
    pub status: String,
    pub scope_label: String,
    pub customer_path: String,
    pub line_count: usize,
    /// 最近加入该草稿的批次条目（按 batch id DESC 最多 8 条；空单 = 空 Vec）。
    ///
    /// 2026-08-22 新增：原只有 `line_count` 总数，前端草稿卡片要直接展示
    /// 「最近加入序列号/名称/订单号」又不想 N 次额外 GET，于是 DTO 一次性
    /// 把这些字段塞过来。`order_no` 是 Option（工单可能没填）。
    pub recent_items: Vec<RecentItemDto>,
}

/// 草稿卡片里要展示的最近批次条目。
///
/// 2026-08-22：原 `AddedBatchDto` 没有 drawing_no/name/order_no，前端卡片
/// 需要这些字段直接展示（序列号 + 名称 + 订单号），避免每次 N 次
/// GET /notes/{id}。这里独立成一个 DTO，与 added_batches 用 `AddedBatchDto`
/// （极简）解耦。
#[derive(Debug, Clone, Serialize)]
pub struct RecentItemDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    /// 工单序列号；t_part.serial_no 是 nullable → 落 Some/None。
    /// 序列化永远给字符串（None → null），与 ScanDeliveryNoteSummaryDto 内
    /// 其它字段对齐。
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    /// 工单订单号；nullable → Some/None。
    pub order_no: Option<String>,
}

/// 已挂载批次（`added_batches[]`）；跨子件场景 part_id/serial_no 必填。
#[derive(Debug, Clone, Serialize)]
pub struct AddedBatchDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub serial_no: String,
    pub quantity: i32,
}

/// 未就绪 part + 其 B 组候选批次（`unresolved_targets[]`）。
/// 散件场景：单元素；装配件场景：每个未就绪子件一个元素。
#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedTargetDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
    pub available_batches: Vec<AvailableBatchDto>,
}

/// B 组候选批次（`unresolved_targets[i].available_batches[]`）。
/// part 级信息（serial_no/drawing_no/name）在 `UnresolvedTargetDto` 外层，不重复。
#[derive(Debug, Clone, Serialize)]
pub struct AvailableBatchDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub quantity: i32,
    pub status: BatchStatusDto,
}

/// `POST /delivery-notes/scan` 出参（200 OK）。
///
/// 场景 → outcome 映射：
/// - `ADDED`：A 组覆盖所有 target，本次挂载 ≥1 个；`added_batches` 非空，`unresolved_targets = None`
/// - `ALREADY_PRESENT`：A 组覆盖所有 target，但都已在本单（幂等）；二者均空 / None
/// - `CANDIDATES_AVAILABLE`：散件仅 B 组；`unresolved_targets` 单元素
/// - `PARTIAL_ADDED`：装配件 A+B 混合；`added_batches` 是 A 组已挂部分，`unresolved_targets` 是 B 组子件
#[derive(Debug, Clone, Serialize)]
pub struct ScanDeliveryOut {
    pub outcome: ScanOutcomeDto,
    pub resolved: ResolvedEntityDto,
    pub note: ScanDeliveryNoteSummaryDto,

    /// 场景 ①、③、④-已挂载部分；其余场景为 `[]`
    pub added_batches: Vec<AddedBatchDto>,

    /// 场景 ②（单元素）、④（多元素）；其余场景为 `None`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved_targets: Option<Vec<UnresolvedTargetDto>>,
}