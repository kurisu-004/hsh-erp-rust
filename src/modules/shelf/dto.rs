//! shelf 域 DTO（HTTP 请求入参）
//!
//! 对应 Python myERP/schema/shelf.py。
//!
//! 2026-09-22 PR4：原 `dto.rs` 中的出参类型（ShelfOut / ShelfListOut /
//! ShelfForReturnItem / ShelfForReturnOut / ShelfForInspectionItem /
//! ShelfForInspectionOut）已迁移至 `vo/shelf.rs`。
//!
//! 2026-10-02 域拆分：`SetShelfProcessesRequest` / `SetShelfProcessesItem` 与
//! 仓内 `all mapping` VO 一并迁出到 `src/modules/prod/shelf_process/{dto,vo}.rs`
//! （工序映射随端点搬到 prod 域），本文件只剩纯 `t_shelf` 入参。
//!
//! ## `zone` 业务约束
//! `PRODUCTION` / `INSPECTION`（DB varchar，应用层用 enum 校验）。
//!
//! ## `display_order`
//! 物理顺序（0 = 未设置；manager 在 ShelfList 后台手填）。

use serde::Deserialize;

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
///
/// 2026-10-02：`next_process_id` 的「process 存在性占位校验」已从 service 删除
/// （校验结果被立刻丢弃、且是一次跨域多余查询；该语义本就由 worker-scan 后端强
/// 校验）。字段保留：picker 前端仍会把它与候选 shelf 一并提交给 worker-scan。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShelfForReturnQuery {
    #[serde(default)]
    pub next_process_id: Option<String>,
}
