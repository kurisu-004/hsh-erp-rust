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
//! 2026-10-12 新增的 `?date=` 同理（ chrono 解析失败即提取器层 400），**同属这一档**，
//! 不是 40001。

use serde::Deserialize;

/// 两个端点共用的 Query 参数。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PartListQuery {
    /// 小程序 `date-nav-bar` 选中的日期，`YYYY-MM-DD`。serde 走 chrono 的
    /// `NaiveDate` 反序列化，**解析不出日期**的串在 axum `Query` 提取器层就被拒，
    /// 返回 **HTTP 400 纯文本**（与 `?page=abc` 同一档，**不走** `R<T>` 信封）。
    ///
    /// ⚠️ 2026-10-12 **实测**：chrono **不要求**月/日零填充，`?date=2026-8-4`
    /// 是**合法**的（解析成 2026-08-04）。刻意不额外收紧 —— 把宽格式打成 400
    /// 比「格式宽松」严重得多。回归：`tests/wx/part_list.rs::
    /// malformed_date_is_rejected_by_the_query_extractor`。
    ///
    /// 缺省 = 不加日期谓词（沿用本仓 `$n::T IS NULL` 惯用法，见
    /// [`super::repo`] 的 3 个日期片段常量）。
    ///
    /// 2026-10-12 新增：此前该页的日期栏是**纯装饰**的 —— `selectedDate`
    /// 既不进 `queryKey` 也不进 `queryFn`，后端也没有日期参数。本字段把它接成
    /// 真筛选，谓词打的是 `p.system_delivery_date`（**不是** `planned_delivery_date`）。
    #[serde(default)]
    pub date: Option<chrono::NaiveDate>,
    /// 前端 tab 值，白名单 **7 个**：`all` / `pendingProduction` /
    /// `inProduction` / `outsource` / `inspecting` / `delivered` /
    /// `noSystemDate`。缺省 = `all`。
    ///
    /// ⚠️ **不是** DB 状态值。映射表在 [`super::service`]（私有），
    /// 白名单外的值一律 `AppError::validation`（40001 / HTTP 422）。
    ///
    /// ⚠️ 2026-10-12 **语义变更**：`all` / 缺省**不再是「不过滤」**，一律落到
    /// 6 状态白名单（`PENDING` / `IN_PROCESS` / `OUTSOURCE` / `INSPECTION` /
    /// `READY_TO_SHIP` / `DELIVERED`），从而排除 `PROGRAMMING` / `COMPLETED` /
    /// `CANCELLED`。`noSystemDate` 是 6 状态里 `system_delivery_date IS NULL`
    /// 的那部分，**忽略** `?date=`。
    #[serde(default)]
    pub status: Option<String>,
    /// 页码，从 1 起；缺省 1，`max(1)`（0 与负数都归 1）
    #[serde(default)]
    pub page: Option<i64>,
    /// 每页条数；缺省 10，`clamp(1, 50)`
    #[serde(default)]
    pub size: Option<i64>,
}
