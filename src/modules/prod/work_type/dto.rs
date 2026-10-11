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
    /// 2026-10-15 新增：自动补料上限。缺省 = 未配置（自动路径兜底
    /// `max_held_batches`）。有值须 `1..=max_held_batches`（service 层校验）。
    #[serde(default)]
    pub auto_allocate_limit: Option<i32>,
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
    /// `max_held_batches` 三态：`None` ⇒ 不动；`Some(None)` ⇒ 清空（SET NULL）；
    /// `Some(Some(v))` ⇒ 改值。
    ///
    /// ⚠️ **2026-10-15：补 `deserialize_some`，让「显式清空」真正可达** ——
    /// 此前只有 `#[serde(default)]` 时，JSON `null` 被 serde 折成外层 `None`
    /// （与「字段缺省」不可区分），`Some(None)` 分支从未被触发过。本次为支撑
    /// `auto_allocate_limit` 的「合并三态校验矩阵」（清 max 时要看过 auto 是否
    /// 还有值）一并修正；`null` 由「不修改」变为「清空」是一次 Wire 行为变更。
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_some")]
    pub max_held_batches: Option<Option<i32>>,
    /// 2026-10-15 新增（三态，同 `max_held_batches`）：`None` ⇒ 不动；
    /// `Some(None)` ⇒ 清空（回到「自动路径兜底 max」）；`Some(Some(v))` ⇒ 改值。
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_some")]
    pub auto_allocate_limit: Option<Option<i32>>,
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
