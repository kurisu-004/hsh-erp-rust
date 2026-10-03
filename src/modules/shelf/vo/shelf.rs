//! shelf 域主货架端点响应 VO

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::serialize_i64;

/// 货架详情出参。
///
/// 2026-09-22 PR4：迁移到 vo/，仅 Serialize。
/// 2026-10-02 域拆分：原 `account_count` 字段删除 —— 货架域对账号的唯一耦合就是它
/// （喂 `t_user_role WHERE scope_type='shelf'` 的 GROUP BY 计数），而绑定真源本来
/// 就在 iam 域（`t_user_role`）。用户决定舍弃该字段、前端不再显示，故连同
/// `ShelfRepo::count_accounts_by_shelf` 一并移除，**本任务零 iam 模块改动**。
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
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 货架列表出参（分页）。字段顺序对齐 Python `schema/shelf.py::ShelfListOut`，
/// 前端翻页需要 limit/offset 回显，故不复用 `shared::response::Page<T>`。
///
/// 2026-09-22 PR4：迁移到 vo/。
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
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfForReturnItem {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    /// 货架负载 = `t_part_batch` 里 `current_holder_id = 本架` 且
    /// `status IN ('PENDING','IN_PROCESS','INSPECTION','OUTSOURCE')` 且
    /// `deleted_at IS NULL` 的批次 **quantity 总和**（件数口径，不是批次数）。
    /// 口径与 `ShelfForInspectionItem::current_load` 逐字一致。
    pub current_load: i64,
    pub is_recommended: bool,
}

/// for-return picker 整体响应（仅返回 `items[]`，不分页；量小）。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfForReturnOut {
    pub items: Vec<ShelfForReturnItem>,
}

/// for-inspection picker 出参：仅 `zone='INSPECTION' AND is_active=true`。
///
/// 2026-09-22 PR4：迁移到 vo/。
/// `current_load` 是**必出**字段：前端品检架卡片无 `v-if` 守卫地渲染「在架 N 件」，
/// 本字段是其唯一数据源，缺字段则每张送检架卡片显示「在架 **undefined** 件」。
/// 聚合在后端补齐而不是前端加守卫，口径与 `ShelfForReturnItem::current_load` 逐字
/// 一致（同 status 列表、同 `SUM(quantity)`、同 `deleted_at IS NULL`）。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfForInspectionItem {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    pub is_active: bool,
    /// 批次 quantity 总和（口径见 `ShelfForReturnItem::current_load`）。
    pub current_load: i64,
}

/// for-inspection picker 整体响应。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfForInspectionOut {
    pub items: Vec<ShelfForInspectionItem>,
}
