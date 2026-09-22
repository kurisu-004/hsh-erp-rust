//! customer 域 DTO（仅入参）
//!
//! 对应 Python myERP/schema/customer.py。
//!
//! ## id 序列化约定
//! 入参 `parent_id` 走字符串（雪花 ID 防 JS 精度截断约定），service 层 `parse::<i64>()`。
//! 出参 VO（`CustomerOut` / `CustomerListOut`）已抽离到 `super::vo`，本文件不再 derive Serialize。
//!
//! 2026-09-22 PR4：出参结构平移到 `vo/customer.rs`，对齐 iam 范本。

use serde::Deserialize;

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
