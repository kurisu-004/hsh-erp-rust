//! worker 域 DTO
//!
//! 对应 Python myERP/schema/worker.py。
//!
//! ## id 序列化约定
//! 裸 `i64` 字段用 `#[serde(serialize_with = "crate::shared::types::serialize_i64")]`。
//! 可空 id（`work_type_id`）在 service 层就转成 `Option<String>`，避免为 `Option<i64>`
//! 再写一套 serde helper——出参 JSON 形态与 Python 完全一致（null 仍是 null）。
//!
//! ## 入参格式校验
//! `id_card_no` / `phone` 仅做 trim + 长度上限，不做严格正则（与 Rust 现状对齐；
//! Python `field_validator` 用正则校验，本域 Rust 实现先做基础校验，业务严格度
//! 后续可对齐）。空串 / 全空白视为 `None`（与 Python `exclude_unset` + strip 一致）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

// ---------------------------------------------------------------------------
// 出参
// ---------------------------------------------------------------------------

/// 工人详情出参。`work_type_name` 由 service 层用 `WorkTypeRepo::list_by_ids`
/// 单条 SQL 批量补全（防 N+1）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub badge_code: String,
    pub name: String,
    pub id_card_no: Option<String>,
    pub phone: Option<String>,
    pub is_active: bool,
    pub work_type_id: Option<String>,
    pub work_type_name: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 工人列表出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerListOut {
    pub items: Vec<WorkerOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// 校验工牌请求体（扫码台用）。
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyBadgeRequest {
    pub badge_code: String,
}

/// 创建工人。
///
/// - `badge_code` 业务唯一键（`uk_t_worker_badge_code`，活跃行唯一）；空 → 20104
/// - `name` 必填 trim 非空
/// - `id_card_no` / `phone` / `work_type_id`：可选；空串视为 None
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerCreateRequest {
    pub badge_code: String,
    pub name: String,
    #[serde(default)]
    pub id_card_no: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub work_type_id: Option<String>,
}

/// 部分更新（OCC）：未提供的字段保持原值（与 Python `exclude_unset` 语义对齐）。
///
/// - `work_type_id` 三态编码 `Option<Option<String>>`：
///   - `None` ⇒ 字段缺省，不修改
///   - `Some(null)` ⇒ 显式清空（SET NULL）
///   - `Some(value)` ⇒ 改值（service 层 `parse::<i64>()`）
/// - `id_card_no` / `phone` 用 `Option<Option<String>>` 同形（用于「清空」语义）
/// - `name` / `badge_code` 一态 `Option<String>`（None = 不改；空串 = 显式拒）
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkerUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub badge_code: Option<String>,
    #[serde(default)]
    pub id_card_no: Option<Option<String>>,
    #[serde(default)]
    pub phone: Option<Option<String>>,
    #[serde(default)]
    pub work_type_id: Option<Option<String>>,
}

/// 列表查询参数：`name_like` / `is_active` 过滤 + 分页。
///
/// - `name_like`：ILIKE '%needle%'，trim 后空串视为无过滤
/// - `is_active`：精确匹配；缺省 = 不过滤
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkerListQuery {
    #[serde(default)]
    pub name_like: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}
