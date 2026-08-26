//! shelf 域 DTO
//!
//! 对应 Python myERP/schema/shelf.py。
//!
//! ## id 序列化约定
//! 裸 `i64` 字段用 `#[serde(serialize_with = "crate::shared::types::serialize_i64")]`。
//! 可空 id 在 service 层就转成 `Option<String>`。
//!
//! ## `zone` 业务约束
//! `PRODUCTION` / `INSPECTION`（DB varchar，应用层用 enum 校验）。
//!
//! ## `display_order`
//! 物理顺序（0 = 未设置；manager 在 ShelfList 后台手填）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

// ---------------------------------------------------------------------------
// 出参
// ---------------------------------------------------------------------------

/// 货架详情出参。`account_count` 由 service 层用 `count_accounts_by_shelf`
/// 单条 GROUP BY SQL 批量补全（防 N+1）。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    pub is_active: bool,
    pub display_order: i32,
    pub account_count: i64,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 货架列表出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfListOut {
    pub items: Vec<ShelfOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// for-return picker 出参：PRODUCTION 区活跃货架，按 `current_load` 升序，
/// `is_recommended = true` 标在最空（load 最小）的那条；其余 false。
///
/// `next_process_id` 仅占位：picker 页面会传给 worker-scan 让后端再次校验
/// 该 process 是否被该货架映射。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfForReturnItem {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    pub current_load: i64,
    pub is_recommended: bool,
}

/// for-return picker 整体响应（仅返回 `items[]`，不分页；量小）。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfForReturnOut {
    pub items: Vec<ShelfForReturnItem>,
}

/// for-inspection picker 出参：仅 `zone='INSPECTION' AND is_active=true`。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfForInspectionItem {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShelfForInspectionOut {
    pub items: Vec<ShelfForInspectionItem>,
}

/// 单个 shelf ↔ process 映射行（按 sort_order）。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfProcessMappingItem {
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub shelf_code: String,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShelfProcessMappingOut {
    pub items: Vec<ShelfProcessMappingItem>,
}

/// 所有 shelf ↔ process 映射（GET /shelves/processes 的批量查询返回）。
///
/// 用途：part_batch / worker_pool 在创建批次/工人时一次性拿全 active shelf 的
/// 工序映射，避免 N+1。
#[derive(Debug, Clone, Serialize)]
pub struct AllShelfProcessMappingItem {
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub shelf_code: String,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AllShelfProcessMappingOut {
    pub items: Vec<AllShelfProcessMappingItem>,
}

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// 创建货架。
///
/// - `code` 业务唯一键（uk_t_shelf_code，活跃行唯一）；缺省/空 → 20104
/// - `zone` ∈ {PRODUCTION, INSPECTION}；其他值 → 20104
/// - `location` / `display_order`：可选
#[derive(Debug, Clone, Deserialize)]
pub struct ShelfCreateRequest {
    pub code: String,
    pub name: String,
    pub zone: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub display_order: Option<i32>,
}

/// 部分更新：未提供的字段保持原值（与 Python `exclude_unset` 语义对齐）。
///
/// - `location` 三态：`None` ⇒ 缺省不改；`Some(null)` ⇒ 清空；`Some(v)` ⇒ 改
/// - `display_order`：None ⇒ 不改；Some(v) ⇒ 改
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShelfUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub location: Option<Option<String>>,
    #[serde(default)]
    pub display_order: Option<i32>,
}

/// 列表查询参数：`code_like` / `zone` / `is_active` 过滤 + 分页。
///
/// - `code_like`：ILIKE '%needle%'，trim 后空串视为无过滤
/// - `zone`：精确匹配（PRODUCTION / INSPECTION）；trim 后空串视为无过滤
/// - `is_active`：精确匹配；缺省 = 不过滤
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShelfListQuery {
    #[serde(default)]
    pub code_like: Option<String>,
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// for-return picker 查询参数：`next_process_id` 必填（worker 当前持有
/// 批次的下一道工序，决定哪些货架可用 —— 仅映射了该工序的货架候选）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShelfForReturnQuery {
    #[serde(default)]
    pub next_process_id: Option<String>,
}

/// set shelf processes 入参：整组替换（先软删全部旧 mapping → INSERT 新列表）。
///
/// `items` 可为空数组（= 清空映射）。每个 `{process_id, sort_order}` 的
/// `process_id` 必须现存，否则 service 层抛 20505。
#[derive(Debug, Clone, Deserialize)]
pub struct SetShelfProcessesRequest {
    pub items: Vec<SetShelfProcessesItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetShelfProcessesItem {
    #[serde(default)]
    pub process_id: String,
    #[serde(default)]
    pub sort_order: i32,
}