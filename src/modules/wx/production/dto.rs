//! wx::production 子模块入参 DTO 层（仅 `Deserialize`）
//!
//! 2026-10-11 新增。
//!
//! ## 两个端点共用同一份 Query
//! `GET /api/v2/wx/production`（首屏聚合）与 `GET /api/v2/wx/production/page`
//! （上拉增量）的 query 语义完全一致，故共用 [`ProductionQuery`]，避免两个端点
//! 悄悄漂移出不同口径（回归：
//! `tests/wx/production.rs::page_endpoint_matches_home_endpoint_at_same_page`）。
//!
//! ## ⚠️ `tab` 是**必填**字段（缺字段与传非法值是两种不同的失败）
//! - **缺 `?tab=`** → serde 缺字段 ⇒ axum `Query` 提取器拒绝 ⇒ **HTTP 400 纯文本
//!   body**（**不走** `R<T>` 信封）。这是旧 `GET /wx/batches` 的**原样行为**，本轮
//!   不改：查无「tab 缺省」的前端调用方（生产页首屏固定带 tab）。
//! - **传了但不在白名单** → service 层 `AppError::validation` ⇒ **40001 /
//!   HTTP 422**，走 `R<T>` 信封。
//!
//! 两者的区别登记在 `docs/api/wx.md` §4；别把「400 纯文本」那行写成 422。
//!
//! ## ❌ 没有 `status=`（`part_list` 有，本域没有）
//! 本域的 tab 值就叫 `tab`（前端 `FetchBatchesOptions.tab`），与 `part_list` 的
//! `status=` 是**两个不同参数名**。刻意不统一：小程序两页的 service 层各自发各自
//! 的参数名，改名即打断契约。

use serde::Deserialize;

/// 两个端点共用的 Query 参数。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProductionQuery {
    /// 前端 tab 值：**`in_progress`** / **`done`**（必填，缺省 → HTTP 400 纯文本）。
    ///
    /// ⚠️ **不是** DB 状态值（传 `IN_PROCESS` / `DELIVERED` 会被拒）。映射表在
    /// [`super::service`]（私有），白名单外的值一律 `AppError::validation`
    /// （40001 / HTTP 422）。
    ///
    /// 刻意**不**复用 `part::statemachine::PartStatus` 做校验 —— 那正是本次
    /// 重构要消灭的跨域复用。
    pub tab: String,
    /// 统计月份 `YYYY-MM`；缺省 = 当前月（`chrono::Local::now() %Y-%m`）。
    #[serde(default)]
    pub period: Option<String>,
    /// 页码，从 1 起；缺省 1，`max(1)`（0 与负数都归 1）
    #[serde(default)]
    pub page: Option<i64>,
    /// 每页条数；缺省 10，`clamp(1, 50)`
    #[serde(default)]
    pub size: Option<i64>,
}
