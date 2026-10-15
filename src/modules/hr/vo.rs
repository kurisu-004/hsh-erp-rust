//! `hr` 域出参 VO（**2026-10-19 新增**）—— **全部由后端算好**，前端不重复实现推导。
//!
//! ## 三个派生列的出处（班次出参）
//!
//! ```
//! cross_day        = max(end) > "24:00"
//! overflow_minutes = max(end) − 24:00        # 0 表示不跨天
//! daily_minutes    = Σ(end − start)          # 无折算分支
//! ```
//!
//! ⚠️ `daily_minutes` 是**各段时长之和**，**不按是否跨天折算** —— 段已经是绝对时钟
//! 轴上的偏移，`26:00 − 22:00` 天然就是 4 小时，再折算一次是双重记账。
//!
//! ⚠️ `overflow_minutes` 用**分钟**而不是小时：段值允许 `:30`（`26:30` = 次日 02:30），
//! 截断成整小时会把 90 分钟的溢出报成 1 小时。`cross_day` 已经能表达「有没有跨天」
//! 这个布尔事实，要小时显示由前端自己四舍五入。
//!
//! ## 雪花 ID 一律序列化成 JSON string
//!
//! `shared::types::serialize_i64`（防 JS `Number.MAX_SAFE_INTEGER` 精度截断），
//! 与仓内既有出参逐字同款。

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::modules::hr::model::{DayType, ShiftSegment};
use crate::shared::types::serialize_i64;

// ============================================================================
// 端点 1：全局日历区间读
// ============================================================================

/// 日历区间读的**出参信封内层**（`items` 逐日一行，一天都不省）。
#[derive(Debug, Clone, Serialize)]
pub struct CalendarDaysOut {
    pub items: Vec<CalendarDayOut>,
}

/// 一天一行。
#[derive(Debug, Clone, Serialize)]
pub struct CalendarDayOut {
    pub cal_date: NaiveDate,
    /// 该日的日型（**已含按星期推导的兜底**，前端不需要再自己判周六周日）。
    pub day_type: DayType,
    /// 节假日名 / 休息日说明；`derived = true` 的行恒为 `null`（库里就没这行）。
    pub name: Option<String>,
    /// `true` = 该日**无显式行**，值是按星期推导出来的（常态工作日 / 常态周末）。
    ///
    /// ⚠️ 前端据此区分「这天的类型是有人在管」与「这天的类型是系统兜底」——
    /// 后者在排班表上不应该显示成一条「被配置过」的标记。
    pub derived: bool,
}

/// 全局日历批量写端点（upsert / delete 共用）的出参。
#[derive(Debug, Clone, Serialize)]
pub struct CalendarDaysWriteOut {
    /// 实际改动行数（upsert = `INSERT … ON CONFLICT DO UPDATE` 命中行数；
    /// delete = 被软删的显式行数）。
    ///
    /// ⚠️ delete 的 `affected = 0` 是**合法成功**（那几天本来就没有显式行，
    /// 删完本来就该是推导值），不是错误。
    pub affected: i64,
}

// ============================================================================
// 端点 4 / 5 / 6 / 7：班次
// ============================================================================

/// 班次列表出参（端点 4）。
#[derive(Debug, Clone, Serialize)]
pub struct ShiftListOut {
    pub items: Vec<ShiftOut>,
}

/// 班次出参（端点 4 / 5 / 6 / 7 复用同一形状）。
#[derive(Debug, Clone, Serialize)]
pub struct ShiftOut {
    /// 雪花 ID（序列化为 **string**）。
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// OCC 锚：`POST /shifts/{id}` / `POST /shifts/{id}/deactivate` 的 body 必须回传它。
    pub version: i32,
    pub code: String,
    pub name: String,
    pub color: Option<String>,
    /// 时段（**后端已按 `start` 升序规范化 + 重编 `seq`**，前端直接渲染即可）。
    pub segments: Vec<ShiftSegment>,
    pub is_active: bool,
    pub sort_order: i32,
    pub remark: Option<String>,
    // ── 三个派生列（见本文件模块 doc）──
    /// 是否跨天（`max(end) > "24:00"`）。
    pub cross_day: bool,
    /// 跨出去的分钟数；`0` 表示不跨天（与 `cross_day` 同真同假）。
    pub overflow_minutes: i32,
    /// 各段时长之和（分钟，无折算）。
    pub daily_minutes: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

// ============================================================================
// 端点 8 / 9 / 10：班次日历
// ============================================================================

/// 班次区间日历出参（端点 8）。
#[derive(Debug, Clone, Serialize)]
pub struct ShiftCalendarOut {
    /// 所属班次 id（序列化为 string）—— 让前端不必为了画个标题再回查一次列表。
    #[serde(serialize_with = "serialize_i64")]
    pub shift_id: i64,
    pub items: Vec<ShiftCalendarDayOut>,
}

/// 班次日历里的一天。
#[derive(Debug, Clone, Serialize)]
pub struct ShiftCalendarDayOut {
    pub cal_date: NaiveDate,
    /// **全局日历**说的这天的日型（无显式行时已按星期推导过）。
    pub global_day_type: DayType,
    /// 该班次**真正生效**的日型（覆盖行 > 全局行 > 星期推导）。
    pub effective_day_type: DayType,
    /// `effective_day_type` 是否算上班日（考勤取数判据）。
    pub is_workday: bool,
    /// 是否来自本班次的覆盖行。
    ///
    /// ⚠️ 它表达的是「**有没有覆盖行**」，不是「值是否与全局相同」——
    /// 显式写一条与全局同值的覆盖行也是允许的（用来锁住「将来有人改全局日历不影响
    /// 这个班次」）。故判据只能是覆盖行的存在性。
    pub is_override: bool,
}

/// 班次覆盖行批量写端点（upsert / delete 共用）的出参。
#[derive(Debug, Clone, Serialize)]
pub struct ShiftDaysWriteOut {
    pub affected: i64,
}
