//! process 域 DTO（入参 + 校验）
//!
//! 对应 Python myERP/schema/process.py。
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 出参结构（`ProcessOut` / `ProcessListOut`）已抽离至 `super::vo`。
//! 本文件仅含入参（Deserialize）。

use serde::Deserialize;

use crate::shared::types::deserialize_some;

/// 创建工序。
///
/// - `code` 业务唯一键（uk_t_process_code，活跃行唯一）；缺省/空 → 20104
/// - `category` ∈ {INHOUSE, OUTSOURCE}；其他值 → 20104
/// - `requires_approval`：OUTSOURCE 保留请求值（默认 true）；INHOUSE service 层强制 false
/// - `color`：`#RRGGBBAA` 9 字符；空串/null ⇒ NULL（不设色）；非空但格式不对 ⇒ 20104
#[derive(Debug, Clone, Deserialize)]
pub struct ProcessCreateRequest {
    pub code: String,
    pub name: String,
    pub category: String,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub requires_approval: Option<bool>,
    #[serde(default)]
    pub color: Option<String>,
}

/// 部分更新：未提供的字段保持原值（与 Python `exclude_unset` 语义对齐）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProcessUpdateRequest {
    /// 仅用作「拒绝」哨兵：客户端若传 `code` 字段一律 20104 BIZ_INVALID_VALUE
    /// （业务唯一键不可变）。缺省时 `None` = 客户端未传 = 通过。
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub description: Option<Option<String>>,
    #[serde(default)]
    pub requires_approval: Option<bool>,
    /// 三态：`None` ⇒ 缺省不改；`Some(null)` ⇒ 显式清空；`Some("...")` ⇒ 改值
    #[serde(default, deserialize_with = "deserialize_some")]
    pub color: Option<Option<String>>,
}

/// 列表查询参数：`code_like` / `category` 过滤 + 分页。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProcessListQuery {
    #[serde(default)]
    pub code_like: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}
