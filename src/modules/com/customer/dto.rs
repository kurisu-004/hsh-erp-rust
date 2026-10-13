//! customer 域 DTO（仅入参）
//!
//! 对应 Python myERP/schema/customer.py。
//!
//! ## id 序列化约定
//! 入参 `parent_id` 走字符串（雪花 ID 防 JS 精度截断约定），service 层 `parse::<i64>()`。
//! 出参 VO（`CustomerOut` / `CustomerListOut`）已抽离到 `super::vo`，本文件不再 derive Serialize。
//!
//! 2026-09-22 PR4：出参结构平移到 `vo/customer.rs`，对齐 iam 范本。

use serde::Deserialize;

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// 创建客户：name 必填；根（`parent_id` 缺省）必须带 `serial_prefix`
/// （1 个大写字母），非根必须带 `parent_id`。
///
/// 2026-10-17：层级由固定两层放开为**任意深度**，两条判据本身深度无关（判的是
/// 「有没有 parent_id」）；父节点可以是任意层级的客户，但必须已存在且未软删
/// （service 层校验，否则造断链）。
///
/// `parent_id` 以字符串形式入参（雪花 ID 防 JS 精度截断约定），service 层 `parse::<i64>()`。
#[derive(Debug, Clone, Deserialize)]
pub struct CustomerCreateRequest {
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub serial_prefix: Option<String>,
}

/// 部分更新：未提供的字段保持原值（与 Python `exclude_unset` 语义对齐）。
///
/// `parent_id` / `serial_prefix` 用 `Option<Option<String>>` 三态编码：
/// - 字段缺省 ⇒ `None` ⇒ 不修改
/// - `Some(null)` ⇒ 显式清空（仅 `serial_prefix`，**根**客户才能传）
/// - `Some(value)` ⇒ 改值
///
/// ⚠️ `parent_id` **一律不支持修改**（字段保留在 DTO 里只为让显式传值能拿到一条
/// 明确文案，而不是静默忽略）。2026-10-17 起这是**产品决定**、不是「暂未实现」：
/// 客户层级不可移动，要换父节点请删除后在目标父节点下重建。理由见
/// `service/crud.rs::update_customer` 的注释。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CustomerUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent_id: Option<Option<String>>,
    #[serde(default)]
    pub serial_prefix: Option<Option<String>>,
}

/// 列表查询参数：`name_like` / `parent_id` / `is_root` 三过滤 + 分页。
///
/// - `name_like`：ILIKE '%needle%'，trim 后空串视为无过滤
/// - `parent_id`：精确匹配；与 `is_root` 互斥（同时传则以 `parent_id` 为准）
/// - `is_root`：`Some(true)` ⇒ `parent_id IS NULL`（根）；`Some(false)` ⇒ `parent_id IS NOT NULL`
/// - `limit`：**2026-10-17 起缺省 = 不分页、返回全量**（旧的 `DEFAULT_LIMIT = 50`
///   已删除：前端 `listCustomers()` 不传 limit 并把整份列表当全集用，旧口径下节点一多
///   就静默截断）。显式传值才分页，并被 service 夹到 `[1, 500]`；`offset` 只在
///   `limit` 存在时生效。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CustomerListQuery {
    #[serde(default)]
    pub name_like: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub is_root: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}
