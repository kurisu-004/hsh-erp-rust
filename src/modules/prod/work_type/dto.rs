//! work_type 域 DTO（入参 + 校验）
//!
//! 对应 Python myERP/schema/work_type.py。
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 出参结构（`WorkTypeOut` / `WorkTypeListOut` / `WorkTypeProcessMappingItem` /
//! `WorkTypeProcessMappingOut`）已抽离至 `super::vo`。
//! 本文件仅含入参（Deserialize）。

use serde::Deserialize;

/// 创建工种。
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

/// 部分更新（OCC）：未提供的字段保持原值。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkTypeUpdateRequest {
    /// 仅用作「拒绝」哨兵：业务唯一键不可变。
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
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkTypeListQuery {
    #[serde(default)]
    pub code_like: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// set work_type processes 入参：整组替换。
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
