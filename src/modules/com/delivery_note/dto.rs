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
//! 2026-10-08：删掉手动建单入参 `DeliveryNoteCreateRequest` / 入单条目
//! `DeliveryNoteAddItem` / 添加零件入参 `DeliveryNoteAddPartsRequest` —— 入单入口
//! 收敛为 `POST /scan` 单一入口后，前端不再有「先建单再挂批次」的两段式表单。
//!
//! ## 分段
//! - 送货分组：创建 / 更新 / 软删三组入参
//! - 送货单：版本化 OCC 入参 / partial update / 移除批次 / 领取 / 列表 query /
//!   扫码入单

use chrono::NaiveDate;
use serde::Deserialize;

// ===========================================================================
//  P1：送货分组 DTO（设计 §6.1）
// ===========================================================================

/// 创建分组入参（POST /api/v2/com/delivery/group）
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

/// 更新分组入参（POST /api/v2/com/delivery/group/{id}/update）
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

/// 软删除分组入参（POST /api/v2/com/delivery/group/{id}/soft-delete）
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryGroupIdRequest {
    pub version: i32,
}

// ===========================================================================
//  P2：送货单生命周期 DTO（移植 + 范围字段扩展）
// ===========================================================================

/// 移除零件入参（POST /api/v2/com/delivery/note/{id}/remove-parts）。
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

/// partial update 入参（POST /api/v2/com/delivery/note/{id}/update；DRAFT/SUBMITTED）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteUpdateRequest {
    pub version: i32,
    pub delivery_date: Option<NaiveDate>,
    pub note: Option<String>,
}

/// 领取入参（POST /api/v2/com/delivery/note/{id}/pickup）。
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

/// GET /api/v2/com/delivery/note/batch-detail?ids=... 查询参数。
/// `ids` 为可选；handler 内部做 split/trim/filter/dedupe/parse i64 + 1..=200
/// 校验。这里只声明 query 形状。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteBatchDetailQuery {
    pub ids: Option<String>,
}

/// GET /api/v2/com/delivery/note 查询参数。
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

/// GET /api/v2/com/delivery/note/pickup-pending 查询参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePickupPendingQuery {
    #[serde(
        default,
        deserialize_with = "crate::shared::types::deserialize_i64_opt"
    )]
    pub customer_id: Option<i64>,
}

/// GET /api/v2/com/delivery/note/candidate-parts 查询参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteCandidatePartsQuery {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub customer_id: i64,
}

/// GET /api/v2/com/delivery/note/{id} 路径参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePath {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub id: i64,
}

// ===========================================================================
//  P3：扫码入单 DTO（设计 §5，POST /api/v2/com/delivery/note/scan）
// ===========================================================================

/// 扫码入单请求体。
///
/// `code` 是 trim 后的扫码载荷，长度要求 1..=64 字符；空白 / 空 → 400
/// `BIZ_INVALID_VALUE`。
#[derive(Debug, Clone, Deserialize)]
pub struct ScanDeliveryRequest {
    pub code: String,
}
