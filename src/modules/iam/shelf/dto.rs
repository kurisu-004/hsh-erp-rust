//! 货架子模块 DTO（HTTP 请求入参）
//!
//! 对应 Python myERP/schema/shelf.py。
//!
//! 2026-09-22 PR4：原 `dto.rs` 中的出参类型（ShelfOut / ShelfListOut 等）已迁移至
//! `vo/shelf.rs`。
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
/// - `capacity`（2026-10-10）：负载上限（**件数**）。`None` 或 `<= 0` 一律按
///   「不限」接受，**不报错** —— 加 CHECK 约束反而会把「<= 0 表示不限」这条
///   语义钉死成非法值。选架排序时不限架恒排最后（见
///   `shared::shelf::select::pick_least_loaded`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ShelfCreateRequest {
    pub code: String,
    pub name: String,
    pub zone: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub display_order: Option<i32>,
    /// 负载上限（件数）。缺省 / `null` = 不限。
    #[serde(default)]
    pub capacity: Option<i32>,
}

/// 部分更新：未提供的字段保持原值（与 Python `exclude_unset` 语义对齐）。
///
/// - `location` 三态：`None` ⇒ 缺省不改；`Some(null)` ⇒ 清空；`Some(v)` ⇒ 改
///
/// ⚠️ 这一条**声明了三态但当前不可达**：`Option<Option<String>>` 用裸
///   `#[derive(Deserialize)]` 时，JSON `null` 与「字段缺省」都反序列化成外层
///   `None`，`service/crud.rs` 里 `Some(None) ⇒ 清空` 那个分支走不到。所以今天
///   `{"location": null}` 的实际效果是**不改**。本轮**不改** —— 加上
///   `deserialize_some` 会让「清空」这个动作**第一次开始生效**，那是一次行为变更，
///   要与前端确认过「有没有调用方在依赖当前的 no-op」再动。已知偏差登记见
///   `docs/api/shelves.md`。
/// - `capacity`（2026-10-10）：三态**且真的三态**，靠 `deserialize_some` 让 JSON
///   `null` 至少走到外层 `Some(_)`（见
///   `crate::shared::types::deserialize_some` 的 doc）———
///   `None` ⇒ 不改；`null` ⇒ 变回「不限」；`Some(v)` ⇒ 改上限
/// - `display_order`：None ⇒ 不改；Some(v) ⇒ 改
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShelfUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub location: Option<Option<String>>,
    #[serde(default)]
    pub display_order: Option<i32>,
    /// 三态可分是**必须**的：`capacity` 的三态承载「取消上限」这个动作，合并成
    /// 一个 `Option<i32>` 后「清空」与「不动」不可区分，运营清掉上限保存时会被
    /// 当成没改而静默丢失（与 `outsource` 域 `OutsourceCompanyUpdateRequest` 的
    /// `process_ids` 是同一类问题）。
    ///
    /// 与上方 `location` 的差别就在 `deserialize_some` 这一个属性：没有它，
    /// `Option<Option<T>>` 的三态是假的。
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_some")]
    pub capacity: Option<Option<i32>>,
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
