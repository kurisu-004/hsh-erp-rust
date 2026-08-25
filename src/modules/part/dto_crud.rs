//! part 域 CRUD DTO（Phase PR-CRUD）
//!
//! 命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxOut`：单条详情出参（id 字段用 `#[serde(serialize_with =
//!   shared::types::serialize_i64)]`）
//! - `XxxListItem` / `XxxListOut`：列表分页
//! - `XxxListQuery`：列表查询参数
//!
//! Task 5 会在此文件补齐路由层所需的 endpoint DTO（upload_drawing 出参、
//! response envelope 适配等）；当前 PR（Task 4）只放 service 层直接消费的
//! 入参 + 出参结构，确保 service 文件能编译通过。

use serde::{Deserialize, Serialize};

use crate::modules::part::model::TPart;
use crate::shared::types::deserialize_i64;

/// `POST /parts` 入参：单件创建工单。
#[derive(Debug, Clone, Deserialize)]
pub struct PartCreateRequest {
    pub name: String,
    pub drawing_no: String,
    pub applicant_name: String,
    pub quantity: i32,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    pub is_urgent: bool,
    #[serde(deserialize_with = "deserialize_i64")]
    pub customer_id: i64,
    #[serde(default)]
    pub assembly_id: Option<i64>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts` 出参：完整工单 + 客户冗余字段。
#[derive(Debug, Clone, Serialize)]
pub struct PartDetailOut {
    #[serde(flatten)]
    pub part: TPart,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

impl PartDetailOut {
    /// 由完整 `TPart` + 客户冗余字段构造。
    pub fn from_with_customer_extra(
        part: TPart,
        customer_name: Option<String>,
        l1_customer_name: Option<String>,
    ) -> Self {
        Self {
            part,
            customer_name,
            l1_customer_name,
        }
    }
}

/// `GET /parts` 列表行：`TPart` + 客户冗余字段。
#[derive(Debug, Clone, Serialize)]
pub struct PartListItem {
    #[serde(flatten)]
    pub part: TPart,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

/// `GET /parts` 出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct PartListOut {
    pub items: Vec<PartListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// `GET /parts` 查询参数。
///
/// `customer_id`：单值；service 层用 `expand_customer_id` 展开为 L1+L2 ids。
/// `status` / `statuses`：单值 / 多值互不冲突；service 层二选一传入。
/// `sort_by` 白名单（CREATED_AT / UPDATED_AT / PLANNED_DELIVERY_DATE /
/// REQUEST_DATE / SERIAL_NO / DRAWING_NO / NAME），其它退化为 `CREATED_AT`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PartListQuery {
    #[serde(default)]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub statuses: Option<String>, // 逗号分隔字符串（query string 不支持 Vec 友好）
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>, // "ASC" / "DESC"
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `PUT /parts/{id}` 入参：字段可选 UPDATE。
///
/// `version` 必填（OCC）；其它字段未传 → DB 不动。
#[derive(Debug, Clone, Deserialize)]
pub struct PartUpdateRequest {
    pub version: i32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub applicant_name: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub actual_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
}

/// `POST /parts/batch` 单 item 入参：与 `PartCreateRequest` 字段集对齐，
/// 但 `customer_id` 提到 batch 级别（共享）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartBatchCreateItem {
    pub name: String,
    pub drawing_no: String,
    pub applicant_name: String,
    pub quantity: i32,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    pub is_urgent: bool,
    #[serde(default)]
    pub assembly_id: Option<i64>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/batch` 入参：批量创建（共享 customer_id）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartBatchCreateRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub customer_id: i64,
    pub items: Vec<PartBatchCreateItem>,
}

/// `POST /parts/batch` per-item 失败明细。
///
/// `part_id`：`Some(id)` = INSERT 成功但 detail lookup 失败；
///            `None` = INSERT 本身失败。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchCreateFailure {
    #[serde(serialize_with = "serialize_i64_opt")]
    pub part_id: Option<i64>,
    pub code: i32,
    pub message: String,
    pub item_index: usize,
}

/// `POST /parts/batch` 出参：`created` 与 `failed` 互斥。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchCreateOut {
    pub created: Vec<PartDetailOut>,
    pub failed: Vec<PartBatchCreateFailure>,
}

/// `POST /parts/{id}/deliver` 入参。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeliverRequest {
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/cancel` 入参。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CancelRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/complete` 入参。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompleteRequest {
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/start-repair` 入参。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StartRepairRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

// ===== serde helper =====

fn serialize_i64_opt<S: serde::Serializer>(
    v: &Option<i64>,
    s: S,
) -> Result<S::Ok, S::Error> {
    crate::shared::types::serialize_i64_opt(v, s)
}