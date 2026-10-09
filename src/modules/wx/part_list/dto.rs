//! wx::part_list 子模块入参 DTO 层（仅 `Deserialize`）
//!
//! 2026-10-11 新增。
//!
//! ## 两个端点共用同一份 Query
//! `GET /api/v2/wx/part-list`（首屏聚合）与 `GET /api/v2/wx/part-list/page`
//!（上拉增量）的 query 语义完全一致，故共用 [`PartListQuery`]，避免两个端点
//! 悄悄漂移出不同口径。
//!
//! ## ❌ 没有 `customer_id`（2026-10-11 字段级移除）
//! 旧 `parts.rs::PartsListQuery` 带 `customer_id`（注释写「客户视角筛选备用，
//! 本 PR 不实现跨客户权限隔离」）。它是**零消费者的预留参数**：前端两处调用方
//! （`services/parts.ts` 的 `fetchParts`）从不传，SQL 里那两行
//! `AND ($2::bigint IS NULL OR p.customer_id = $2::bigint)` 恒真。留着它会让
//! 「这个端点支持按客户筛选」成为一个**假承诺**——真接上跨客户隔离时又得回头改
//! 权限模型。已在 `docs/api/wx.md` §6 登记为移除项。
//!
//! ## ⚠️ 非法数值走提取器层 400（不走 `R<T>` 信封）
//! `?page=abc` / `?size=abc` 由 axum `Query` 提取器拒绝，返回 **HTTP 400 纯文本**
//! body（不是 `AppError::validation` 的 40001 / 422）。这是仓库全局行为
//! （`test-support::http::send_raw` 就是为断言这类响应而存在），本域未特殊处理。

use serde::Deserialize;

/// 两个端点共用的 Query 参数。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PartListQuery {
    /// 前端 tab 值：`all` / `pendingProduction` / `inProduction` /
    /// `pendingInspection` / `delivered`。缺省 = 不过滤（等价 `all`）。
    ///
    /// ⚠️ **不是** DB 状态值。映射表在 [`super::service`]（私有），
    /// 白名单外的值一律 `AppError::validation`（40001 / HTTP 422）。
    #[serde(default)]
    pub status: Option<String>,
    /// 页码，从 1 起；缺省 1，`max(1)`（0 与负数都归 1）
    #[serde(default)]
    pub page: Option<i64>,
    /// 每页条数；缺省 10，`clamp(1, 50)`
    #[serde(default)]
    pub size: Option<i64>,
}
