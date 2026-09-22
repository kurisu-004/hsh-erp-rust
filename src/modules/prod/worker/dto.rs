//! worker 域 DTO（入参 + 校验）
//!
//! 对应 Python myERP/schema/worker.py。
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 出参结构（`WorkerOut` / `WorkerListOut`）已抽离至 `super::vo`。
//! 本文件仅含入参（Deserialize）。

use serde::Deserialize;

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
