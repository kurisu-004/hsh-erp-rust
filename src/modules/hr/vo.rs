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
//!
//! ## 端点 1 逐日带的 `shift_bands`（2026-10-23 新增）
//!
//! 全局日历每一行额外带**逐班次色带**（[`ShiftBandOut`]），让前端在全局日历上渲染
//! 「所有班次的颜色色带」时**不必**为每个班次再发一次 `GET /shifts/{id}/calendar`
//! （那是 N+1 次请求）。推导与端点 8 走**同一个** `service::derive_effective`，
//! `is_workday` 恒取 `effective_day_type.is_workday()` ⇒ 两个端点不可能漂移。

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
    /// 该日**逐班次**的色带（2026-10-23 新增）—— 全局日历上渲染「所有班次的颜色色带」
    /// 所需的**每日 × 每班次**口径，见 [`ShiftBandOut`]。
    pub shift_bands: Vec<ShiftBandOut>,
}

/// 全局日历里某一个班次在某一天的色带（**`CalendarDayOut.shift_bands[]` 的元素**）。
///
/// ## 三条口径
///
/// 1. **只含 `is_active = true` 的班次**（停用班次不排人，不该在排班表上占一条色带）。
/// 2. **顺序稳定**：按班次 `sort_order` 升序、再 `code` 升序（`code` 在未软删行内唯一
///    ⇒ 全序）。**同一次响应内**数组顺序稳定 ⇒ 前端**只把它当渲染顺序**用（不必再自己
///    排一次）。⚠️ 但这个顺序**不保证与端点 4 `GET /shifts` 一致**（那边同 `sort_order`
///    按 `id` 兜底）⇒ **配对一律按 `shift_id`**（拿 `shift_id` 建 Map 查表配对，**不是**
///    按数组下标 zip）、不得按下标配对，否则色带会串到别的班次上且无任何报错。
///    完整登记在 `docs/api/hr.md` §4.1.1 第 1 条。
/// 3. ⚠️ **`is_workday = false` 的班次照样出现**，只过滤停用班次、**不**在这里按
///    `is_workday` 过滤 —— 前端要的就是「这天哪些班次在休」，把它滤掉就没有色带了。
///    要不要渲染由前端自己决定。
#[derive(Debug, Clone, Serialize)]
pub struct ShiftBandOut {
    /// 班次雪花 ID（序列化为 **JSON string**，与 [`ShiftCalendarOut::shift_id`] 同款）。
    #[serde(serialize_with = "serialize_i64")]
    pub shift_id: i64,
    /// 该班次这一天**真正生效**的日型（覆盖行 > 全局行 > 星期兜底三级推导）。
    ///
    /// ⚠️ 无覆盖行时它逐字等于同一行 [`CalendarDayOut::day_type`]（那已是全局口径）；
    /// 有覆盖行时它可以与全局**不同**（这正是「全局是法定节假日、但这个班次照常上班」）。
    pub effective_day_type: DayType,
    /// `effective_day_type` 是否算上班日（考勤取数的**唯一**判据）。
    pub is_workday: bool,
    /// 是否来自本班次的覆盖行（判据是**存在性**，不是「值是否与全局相同」）。
    pub is_override: bool,
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
