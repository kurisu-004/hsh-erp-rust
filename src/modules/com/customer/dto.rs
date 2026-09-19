//! customer 域 DTO
//!
//! 对应 Python myERP/schema/customer.py。
//!
//! ## id 序列化约定
//! 裸 `i64` 字段用 `#[serde(serialize_with = "crate::shared::types::serialize_i64")]`。
//! 可空 id（`parent_id`）在 service 层就转成 `Option<String>`，避免为 `Option<i64>`
//! 再写一套 serde helper——出参 JSON 形态与 Python 完全一致（null 仍是 null）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

// ---------------------------------------------------------------------------
// 出参
// ---------------------------------------------------------------------------

/// 客户详情出参。`parent_name` 由 service 层补全（连表查父客户的 name）。
#[derive(Debug, Clone, Serialize)]
pub struct CustomerOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub parent_id: Option<String>,
    pub parent_name: Option<String>,
    pub serial_prefix: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 客户列表出参（不分页：phase 1 仅返回全部；后续加 limit/offset 后再补）。
///
/// 字段顺序对齐 Python `schema/customer.py::CustomerListOut`。
#[derive(Debug, Clone, Serialize)]
pub struct CustomerListOut {
    pub items: Vec<CustomerOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// 创建客户：name 必填；L1 必须带 `serial_prefix`（1 个大写字母），L2 必须带 `parent_id`。
///
/// `parent_id` 以字符串形式入参（雪花 ID 防 JS 精度截断约定），service 层 `parse::<i64>()`。
#[derive(Debug, Clone, Deserialize)]
pub struct CustomerCreateRequest {
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub serial_prefix: Option<String>,
}

/// 部分更新：未提供的字段保持原值（与 Python `exclude_unset` 语义对齐）。
///
/// `parent_id` / `serial_prefix` 用 `Option<Option<String>>` 三态编码：
/// - 字段缺省 ⇒ `None` ⇒ 不修改
/// - `Some(null)` ⇒ 显式清空（仅 `serial_prefix`，L1 客户才能传）；
///   `parent_id` 不允许显式置 NULL（必须从 customer CRUD 里走 soft-delete + 重接）
/// - `Some(value)` ⇒ 改值
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CustomerUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent_id: Option<Option<String>>,
    #[serde(default)]
    pub serial_prefix: Option<Option<String>>,
}

/// 列表查询参数：`name_like` / `parent_id` / `is_root` 三过滤 + 分页。
///
/// - `name_like`：ILIKE '%needle%'，trim 后空串视为无过滤
/// - `parent_id`：精确匹配；与 `is_root` 互斥（同时传则以 `parent_id` 为准）
/// - `is_root`：`Some(true)` ⇒ `parent_id IS NULL`；`Some(false)` ⇒ `parent_id IS NOT NULL`
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CustomerListQuery {
    #[serde(default)]
    pub name_like: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub is_root: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}