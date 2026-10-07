//! com::delivery_note 域 DTO（**仅** `Deserialize` 入参）
//!
//! 出参（响应）VO 全在 `super::vo`。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxQuery` / `XxxPath`：查询 / 路径参数
//! - `XxxRequest` / `XxxItem`：业务入参
//!
//! ## 分段
//! - 送货分组：创建 / 更新 / 软删三组入参
//! - 送货单：扫码入单（`ScanEntryRequest` / `ScanEntry`）、版本化 OCC 入参 /
//!   partial update / 移除批次 / 指定司机 / 领取 / 列表 query / 路径参数
//!
//! ## 2026-10-08 删除的入参
//! 手动建单（`DeliveryNoteCreateRequest` / `DeliveryNoteAddItem`）、添加零件
//! （`DeliveryNoteAddPartsRequest`）、弹窗附挂批次（`AttachBatchesRequest`）、
//! 候选取批（`DeliveryNoteCandidatePartsQuery`）、待司机领取一览
//! （`DeliveryNotePickupPendingQuery`）、送货台逐件扫码核销
//! （`DeliveryNotePickupScanRequest`）—— 入单入口收敛为 `POST /scan` 单一入口后，
//! 前端不再有「先建单 / 先挑批次再挂单 / 逐件核销」这几条并行路径。
//!
//! 打印端点的入参也不在本文件：转发链路上 body 以 `Json<Value>` 原样透传给 python，
//! 字段语义由 python 端 schema 负责。

use chrono::NaiveDate;
use serde::Deserialize;

// ===========================================================================
//  送货分组 DTO
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
//  送货单入参
// ===========================================================================

/// `POST /api/v2/com/delivery/note/scan` 请求体（**唯一**入单入口）。
///
/// ⚠️ **不收批次 version**：分配在服务端事务内完成，读到的就是最新；让客户端回传
/// 一个可能已过期的版本只会制造假的 OCC 冲突。
#[derive(Debug, Clone, Deserialize)]
pub struct ScanEntryRequest {
    /// 扫码串（trim 后用于定位零件 / 装配件 → 上推 L1 → find-or-create 草稿）。
    pub serial_no: String,
    /// 送货单 OCC 锚。扫码树返回的 `draft` 非 null 时**必填**；新建草稿（前端还没
    /// 拿到 version）时可缺省。
    pub note_version: Option<i32>,
    pub entries: Vec<ScanEntry>,
}

/// 一个入单条目（零件按件数、装配件按套数）。
#[derive(Debug, Clone, Deserialize)]
pub struct ScanEntry {
    /// `"ASSEMBLY"` | `"PART"`；其它值 400。
    pub node_kind: String,
    /// 节点 id（零件或装配件）。JSON **字符串**（雪花 id > 2^53）。
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub node_id: i64,
    /// `node_kind = ASSEMBLY` 时必填（套数，> 0）。
    pub sets: Option<i32>,
    /// `node_kind = PART` 时必填（件数，> 0）。
    pub quantity: Option<i32>,
}

/// 移除批次入参（POST /api/v2/com/delivery/note/{id}/remove-batches）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteRemoveBatchesRequest {
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

/// 指定司机入参（POST /api/v2/com/delivery/note/{id}/driver）。
///
/// 校验链：note 存在 + version 一致 + `validate_driver`（见
/// `service/lifecycle.rs::validate_driver`）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNoteDriverRequest {
    pub version: i32,
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub driver_worker_id: i64,
}

/// 领取入参（POST /api/v2/com/delivery/note/{id}/pickup）。
///
/// 2026-10-08 瘦身：**删掉 `driver_worker_id`**。司机改从单据上已指定的
/// `driver_worker_id` 读（指定动作独立成 `POST /{id}/driver`）；未指定 ⇒ 21409，
/// 指定过也会重跑 `validate_driver`（司机可能在指定之后被停用 / 改工种）。
///
/// `badge_code` 保留（司机核销时手输工牌号），当前服务端忽略。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePickupRequest {
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

/// GET /api/v2/com/delivery/note/{id} 路径参数。
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePath {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub id: i64,
}
