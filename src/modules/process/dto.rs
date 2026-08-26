//! process 域 DTO
//!
//! 对应 Python myERP/schema/process.py。
//!
//! ## id 序列化约定
//! 裸 `i64` 字段用 `#[serde(serialize_with = "crate::shared::types::serialize_i64")]`。
//!
//! ## 业务枚举
//! `category` 在 DB 存 `varchar(16)` + CHECK (INHOUSE/OUTSOURCE)；DTO 仍以字符串承载，
//! service 层负责 trim + 大写规范化 + 范围校验。
//!
//! ## `requires_approval` 业务约束
//! INHOUSE ⇒ service 层强制 `false`；OUTSOURCE ⇒ 保留请求值（默认 `true`）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

// ---------------------------------------------------------------------------
// 出参
// ---------------------------------------------------------------------------

/// 工序详情出参。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub category: String,
    pub sort_order: i32,
    pub description: Option<String>,
    pub requires_approval: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 工序列表出参。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessListOut {
    pub items: Vec<ProcessOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// 创建工序。
///
/// - `code` 业务唯一键（uk_t_process_code，活跃行唯一）；缺省/空 → 20104
/// - `category` ∈ {INHOUSE, OUTSOURCE}；其他值 → 20104
/// - `requires_approval`：OUTSOURCE 保留请求值（默认 true）；INHOUSE service 层强制 false
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
}

/// 部分更新：未提供的字段保持原值（与 Python `exclude_unset` 语义对齐）。
///
/// - `code` 字段若传一律 20104（业务唯一键不可变）
/// - `category` 同上：传了即拒
/// - `description` 三态：`None` ⇒ 缺省不改；`Some(null)` ⇒ 显式清空；`Some(v)` ⇒ 改值
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
}

/// 列表查询参数：`code_like` / `category` 过滤 + 分页。
///
/// - `code_like`：ILIKE '%needle%'，trim 后空串视为无过滤
/// - `category`：精确匹配（INHOUSE / OUTSOURCE）；trim 后空串视为无过滤
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