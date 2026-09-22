//! delivery_note 域 DTO
//!
//! 对应 Python myERP/schema/delivery_note.py。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxQuery` / `XxxPath`：查询 / 路径参数
//! - `XxxRequest` / `XxxItem`：业务入参
//!
//! 出参（响应）VO 已拆分到 `super::vo`（2026-09-22 PR4 重构，对齐 iam
//! vo/ 范本）。本文件仅保留 `Deserialize` 入参。
//!
//! ## Phase 范围
//! - **P1**：送货分组（§6.1）
//! - **P2**：送货单生命周期 + 候选入单（不含扫码 P3 / 打印 P4）
//! - **P3**：扫码入单（§5）—— ScanRequest 等

use std::collections::HashMap;

use chrono::NaiveDate;
use serde::Deserialize;

// ===========================================================================
//  P1：送货分组 DTO（设计 §6.1）
// ===========================================================================

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
    #[serde(
        default,
        deserialize_with = "crate::shared::types::deserialize_i64_vec_opt"
    )]
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

/// 领取入参（POST /delivery-notes/{id}/pickup）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePickupRequest {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub driver_worker_id: i64,
    pub badge_code: Option<String>,
    pub version: i32,
}

// ---------------------------------------------------------------------------
//  列表 query DTO
// ---------------------------------------------------------------------------

/// GET /delivery-notes/batch-detail?ids=... 查询参数。
/// `ids` 为可选；handler 内部做 split/trim/filter/dedupe/parse i64 + 1..=200
/// 校验。这里只声明 query 形状。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteBatchDetailQuery {
    pub ids: Option<String>,
}

/// GET /delivery-notes 查询参数。
///
/// `statuses` 是逗号分隔字符串（axum 默认 Query 不支持重复 key）：
/// `?statuses=DRAFT,SUBMITTED`。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteListQuery {
    pub statuses: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::shared::types::deserialize_i64_opt"
    )]
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
    #[serde(
        default,
        deserialize_with = "crate::shared::types::deserialize_i64_opt"
    )]
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

// ===========================================================================
//  attach_batches 入参（POST /delivery-notes/{id}/attach-batches）
// ===========================================================================

/// `POST /api/v2/delivery-notes/{note_id}/attach-batches` 请求体。
///
/// 弹窗勾选若干 A 组批次（INSPECTION / READY_TO_SHIP）一次性 attach 到指定
/// DRAFT 送货单。每个 item 带 `version`（OCC 校验）；后端逐项独立处理：
/// 失败项进入响应 `conflicts` 列表，不中断其它项；最终返回 200。
#[derive(Debug, Clone, Deserialize)]
pub struct AttachBatchesRequest {
    pub batches: Vec<AttachBatchItem>,
}

/// 单个批次入参。
///
/// `batch_id` 用字符串反序列化（与 `t_part_batch.id` 列一致；前端 JSON 用
/// 字符串防 JS 精度截断），`version` 是 t_part_batch 当前乐观锁版本。
#[derive(Debug, Clone, Deserialize)]
pub struct AttachBatchItem {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
}

// 注：原本 dto.rs 中 `use serde::Serialize` / `serialize_i64*` 全部移除，
// 出参类型已迁出至 `super::vo`（2026-09-22 PR4 重构）。