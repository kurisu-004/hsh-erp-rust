//! `hr` 域入参 DTO + 字面量常量（**2026-10-19 新增**）。
//!
//! 出参在 [`crate::modules::hr::vo`]（与入参分家：入参是「前端能传的」，出参是
//! 「后端算好的」，后者带服务端派生列，混在一个文件里会让人以为前端可以传
//! `cross_day` / `is_workday`）。
//!
//! ## 零 PUT / PATCH / DELETE
//!
//! 本仓 HTTP 路由零 PUT/PATCH/DELETE（`wecom` 域 2026-10-13 实测核实），故「改」走
//! `POST /shifts/{id}`、「删」走 `POST /shifts/{id}/deactivate` 与
//! `POST …/days/delete` —— 与 `POST /shifts/{id}` 同款约定。
//!
//! ## 权限点常量
//!
//! 读写分两个权限点，读写**不是**同一个（`hr.view` 给所有要看到排班表的人，
//! `hr.schedule.manage` 只给能改配置的人）。守卫**判在 service 第一行**
//! （`current.require_permission(..)`），handler 不重复校验 —— 范式同
//! `prod::shelf_process` / `prod::work_type`。

use chrono::NaiveDate;
use serde::Deserialize;

use crate::modules::hr::model::ShiftSegment;

// ============================================================================
// 字面量常量
// ============================================================================

/// 读权限点（`GET /calendar/days` / `GET /shifts` / `GET /shifts/{id}/calendar`）。
pub const PERM_VIEW: &str = "hr.view";

/// 写权限点（其余 7 条端点）。
pub const PERM_MANAGE: &str = "hr.schedule.manage";

/// 日历区间的**最大跨度**（含首尾，`from` 到 `to` 之间不超过 366 天）。
///
/// ⚠️ 为什么卡：日历读端点要逐日推导并回传（无显式行时按星期兜底），一个请求灌
/// 10 年就是 3650 个出参项 —— 前端用不上，却已经把 DB 与序列化 CPU 花掉了。
/// 超限 → `21904 BIZ_HR_CALENDAR_RANGE_INVALID`。
pub const MAX_RANGE_DAYS: i64 = 366;

/// 时段条数下限 / 上限（V5：`1 ≤ n ≤ 6`）。
///
/// ⚠️ 上限 6 是**排班表可读性**的约束，不是数据库约束：6 段已经能表达「两班倒 +
/// 三个零散加班段」，再多就没人能在排班表上读懂一天了。DB 侧
/// `ck_t_hr_shift_segments` 同值兜底（防绕过 service 直写库）。
pub const MIN_SEGMENTS: usize = 1;
pub const MAX_SEGMENTS: usize = 6;

/// 段值域上界：`"48:00"` = `2880` 分钟（自 `cal_date 00:00` 起算两整天的偏移）。
///
/// ⚠️ `48:00` 是**允许**的端点（一个 24 小时连做的班次：`08:00` → `32:00` 不跨天界，
/// `00:00` → `48:00` 才算）。真正卡死跨日重叠的是 span ≤ 24h（V6 / `21908`）。
pub const MAX_CLOCK_MINUTES: i32 = 48 * 60;

/// `"24:00"`（当天末尾 / 次日零点）的分钟数；`end > 24:00` 即**跨天**。
pub const DAY_BOUNDARY_MINUTES: i32 = 24 * 60;

/// span 上界（V6）：`max(end) − min(start) ≤ 24:00`。
pub const MAX_SPAN_MINUTES: i32 = 24 * 60;

// ============================================================================
// 1 / 8：区间读端点的 query
// ============================================================================

/// `GET /calendar/days?from&to` 的 query（`ISO 8601` 日期，如 `2026-10-01`）。
#[derive(Debug, Clone, Deserialize)]
pub struct CalendarRangeQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// `GET /shifts/{id}/calendar?from&to` 的 query。
pub type ShiftCalendarRangeQuery = CalendarRangeQuery;

/// `GET /shifts?include_inactive` 的 query。
///
/// ⚠️ **缺省为 `false`**（只列启用中的班次）：停用是「下架」，默认混进列表会让
/// 排班表出现一排编了但没人排的班次。`include_inactive=true` 才一并返回
/// （管理页需要看到停用项才能「重新启用」）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShiftListQuery {
    #[serde(default)]
    pub include_inactive: bool,
}

// ============================================================================
// 2 / 3：全局日历写端点
// ============================================================================

/// `POST /calendar/days` —— 批量 upsert 全局日历显式行。
#[derive(Debug, Clone, Deserialize)]
pub struct CalendarDaysUpsertRequest {
    pub items: Vec<CalendarDayUpsertItem>,
}

/// 一条全局日历显式行。
#[derive(Debug, Clone, Deserialize)]
pub struct CalendarDayUpsertItem {
    pub cal_date: NaiveDate,
    /// 6 档之一（未知 → `21905`）。
    pub day_type: String,
    /// 节假日名 / 休息日说明；`CUSTOM_LEAVE` **必填**（缺失 → `21907`）。
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub remark: Option<String>,
}

/// 批量删除显式行的**目标集合**（`{dates:[…]}` 或 `{from,to}` 二选一）。
///
/// ⚠️ 两形态都必填其中之一、都留空 → `21904`（而不是「删 0 行」的假成功）。
/// `dates` 允许为空数组（同样 `21904`：语义上是「没说要删什么」）。
#[derive(Debug, Clone, Deserialize)]
pub struct CalendarDaysDeleteRequest {
    #[serde(default)]
    pub dates: Option<Vec<NaiveDate>>,
    #[serde(default)]
    pub from: Option<NaiveDate>,
    #[serde(default)]
    pub to: Option<NaiveDate>,
}

// ============================================================================
// 5 / 6 / 7：班次写端点
// ============================================================================

/// `POST /shifts` —— 建班次。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateShiftRequest {
    /// 班次编码（`varchar(30)`，未软删行内唯一；重复 → `21902`）。
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    /// 时段（1..6 段，绝对时钟轴）。**必填、无 `#[serde(default)]`** ——
    /// 缺了就 422 纯文本（axum `Json` 提取器），因为「没有时段的班次」没有意义。
    pub segments: Vec<ShiftSegment>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub remark: Option<String>,
}

/// `POST /shifts/{id}` —— 改班次（**部分覆盖**：body 里给了的字段才改）。
///
/// ⚠️ `version` **必填且无 `#[serde(default)]`**（CLAUDE.md §8：所有对带 `version` 列的
/// 表的写端点必须显式收客户端 `version`）。缺失 → axum `Json` 提取器返
/// **HTTP 422 纯文本**（不是业务信封）；陈旧 → `40901`。
/// ⚠️ `segments` 是**单列覆盖**：传了就整列替换（不做段级 merge），不传就保持原值。
/// ⚠️ 本 DTO **没有** `is_active`：启用 / 停用是两个意图，停用有独立端点。
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateShiftRequest {
    pub version: i32,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub segments: Option<Vec<ShiftSegment>>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub remark: Option<String>,
}

/// `POST /shifts/{id}/deactivate` —— 停用（软下架）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeactivateShiftRequest {
    /// OCC 锚（必填，见 [`UpdateShiftRequest::version`]）。
    pub version: i32,
}

// ============================================================================
// 9 / 10：班次覆盖行写端点
// ============================================================================

/// `POST /shifts/{id}/calendar/days` —— 批量 upsert 班次覆盖行。
#[derive(Debug, Clone, Deserialize)]
pub struct ShiftDaysUpsertRequest {
    pub items: Vec<ShiftDayUpsertItem>,
}

/// 一条班次覆盖行。
///
/// ⚠️ **刻意没有 `name` 列**（`t_hr_shift_calendar` 表也没有）：「这天为什么特殊」
/// 是全局日历的语义，班次覆盖只改「这天算不算上班」，说明走 `remark`。
#[derive(Debug, Clone, Deserialize)]
pub struct ShiftDayUpsertItem {
    pub cal_date: NaiveDate,
    /// 6 档之一（未知 → `21905`）。
    pub day_type: String,
    #[serde(default)]
    pub remark: Option<String>,
}

/// 批量清覆盖的**目标集合**（`{dates:[…]}` 或 `{from,to}` 二选一），
/// 语义与 [`CalendarDaysDeleteRequest`] 同款。
#[derive(Debug, Clone, Deserialize)]
pub struct ShiftDaysDeleteRequest {
    #[serde(default)]
    pub dates: Option<Vec<NaiveDate>>,
    #[serde(default)]
    pub from: Option<NaiveDate>,
    #[serde(default)]
    pub to: Option<NaiveDate>,
}
