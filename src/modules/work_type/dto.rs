//! work_type 域 DTO
//!
//! 对应 Python myERP/schema/work_type.py。
//!
//! ## id 序列化约定
//! 裸 `i64` 字段用 `#[serde(serialize_with = "crate::shared::types::serialize_i64")]`。
//! 可空 id 在 service 层就转成 `Option<String>`，避免为 `Option<i64>` 再写一套 serde helper。
//!
//! ## `WorkTypeOut.process_ids`
//! 列表 / 详情出参附带 `Vec<String>`（序列化时把 i64 转 String 即可），service 层用
//! `WorkTypeProcessRepo::list_by_work_types_batch` 一次性补齐（防 N+1）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

// ---------------------------------------------------------------------------
// 出参
// ---------------------------------------------------------------------------

/// 工种详情出参。`process_ids` 由 service 层用
/// `WorkTypeProcessRepo::list_by_work_types_batch` 单条 SQL 批量补全（防 N+1）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub sort_order: i32,
    pub max_held_batches: Option<i32>,
    /// 该工种被映射的工序 id 列表（JSON 序列化为 `["123", "456"]`）。空 = 未映射任何工序。
    pub process_ids: Vec<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 工种列表出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeListOut {
    pub items: Vec<WorkTypeOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 单个 work_type ↔ process 映射行（按 sort_order）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeProcessMappingItem {
    #[serde(serialize_with = "serialize_i64")]
    pub work_type_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeProcessMappingOut {
    pub items: Vec<WorkTypeProcessMappingItem>,
}

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// 创建工种。
///
/// - `code` 业务唯一键（`uk_t_work_type_code`，活跃行唯一）；缺省/空 → 20104
/// - `name` 必填 trim 非空
/// - `description` 可选；空串视为 NULL
/// - `sort_order` 可选，默认 0
/// - `max_held_batches` 可选；NULL=不限；非空时 ≥1（≤0 → 20104）
#[derive(Debug, Clone, Deserialize)]
pub struct WorkTypeCreateRequest {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub max_held_batches: Option<i32>,
}

/// 部分更新（OCC）：未提供的字段保持原值（与 Python `exclude_unset` 语义对齐）。
///
/// - `code` 字段若传一律 20104（业务唯一键不可变）
/// - `description` 三态编码 `Option<Option<String>>`：
///   - `None` ⇒ 字段缺省，不修改
///   - `Some(null)` ⇒ 显式清空（SET NULL）
///   - `Some(value)` ⇒ 改值（trim 后写）
/// - `max_held_batches` 三态同 `description`；改值时需 `ge=1`
/// - `name` 二态 `Option<String>`（None = 不改；空串 = 显式拒）
/// - `sort_order` 二态 `Option<i32>`（None = 不改；Some(v) = 改值）
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkTypeUpdateRequest {
    /// 仅用作「拒绝」哨兵：客户端若传 `code` 字段一律 20104 BIZ_INVALID_VALUE
    /// （业务唯一键不可变）。缺省时 `None` = 客户端未传 = 通过。
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<Option<String>>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub max_held_batches: Option<Option<i32>>,
}

/// 列表查询参数：`code_like` 过滤 + 分页。
///
/// - `code_like`：ILIKE '%needle%'，trim 后空串视为无过滤
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkTypeListQuery {
    #[serde(default)]
    pub code_like: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// set work_type processes 入参：整组替换（先软删全部旧 mapping → INSERT 新列表）。
///
/// `items` 可为空数组（= 清空映射）。每个 `{process_id, sort_order}` 的
/// `process_id` 必须现存，否则 service 层抛 20801 `BIZ_PROCESS_NOT_FOUND`。
#[derive(Debug, Clone, Deserialize)]
pub struct SetWorkTypeProcessesRequest {
    pub items: Vec<SetWorkTypeProcessesItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetWorkTypeProcessesItem {
    #[serde(default)]
    pub process_id: String,
    #[serde(default)]
    pub sort_order: i32,
}