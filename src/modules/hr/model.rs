//! `hr`（人事排班）域模型（**2026-10-19 新增**）：`DayType` 日型枚举 +
//! `ShiftSegment` 时段值对象 +
//! 三张表的**全列** `FromRow` 投影。
//!
//! 仓内惯例：`pub struct T*` 全部 derive `sqlx::FromRow`、全列投影、零例外
//! （同 `wecom::model` / `scheduled_task::model`）。本域 owned 三张表：
//! [`TCalendarDayRow`] ← `t_hr_calendar_day`、
//! [`TShiftRow`] ← `t_hr_shift`、
//! [`TShiftCalendarRow`] ← `t_hr_shift_calendar`。
//!
//! ## 日型推导规则是本域的**唯一真源**
//!
//! ```
//! effective_day_type(shift, date) =
//!     班次覆盖行(day_type)                             若存在
//!   | 全局日历行(day_type)                             若存在
//!   | 按星期推导（周六周日 = REST_DAY，否则 WORKDAY）     兜底
//!
//! is_workday = day_type ∈ { WORKDAY, MAKEUP_WORKDAY }
//! ```
//!
//! ⚠️ **前端不重复实现这套推导** —— 读端点（`GET /calendar/days` /
//! `GET /shifts/{id}/calendar`）已经在服务端把每一天的 `day_type` / `effective_day_type`
//! 算好回传，前端只渲染。
//!
//! ## 时段是**绝对时钟轴**，不是「跨天标志 + 时分」
//!
//! 段值域是 `cal_date 00:00` 起算的**绝对偏移**，字符串 `"HH:MM"`，`HH ∈ 0..48`：
//! 夜班 22:00 → 次日 02:00 写作 `start="22:00"` / `end="26:00"`。
//! **「跨天班次的日期归属 = 工作开始时间所在日」由数据结构本身保证** ——
//! 归属日就是 `cal_date`，不需要任何「跨天标志位 + 推断」逻辑。

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::shared::error::{AppError, code};

// ============================================================================
// DayType —— 6 档日型
// ============================================================================

/// 日型（**无 DB ENUM**，DB 里是 `varchar(24)`，合法性只在本枚举）。
///
/// wire 值与 DB 字面量**逐字相同**（`WORKDAY` / `MAKEUP_WORKDAY` / …），
/// 前端拿到的 `day_type` 就是 DB 里那一列。
///
/// ⚠️ **`Serialize` / `Deserialize` 手工实现，委托给 [`DayType::as_str`] /
///
/// [`DayType::parse`]** —— 刻意**不用** `#[derive(Serialize)]` +
/// `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]`：那要让「wire 字面量」有两份定义
/// （枚举变体名的机械转写 + `as_str()`），两者今天恰好一致、明天有人加一档变体时
/// 就可能只改一处，而**前端看到的日型会静默变形**（`RestDay` vs `REST_DAY`）。
/// 委托给 `as_str()` 之后只有一个真源，加档时忘了改 `as_str()` 会当场编译不过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayType {
    /// 工作日（`is_workday = true`）。
    Workday,
    /// 调休上班（`is_workday = true`；法定节假日调成上班日）。
    MakeupWorkday,
    /// 正常周末（`is_workday = false`）。也是**按星期推导**的兜底值。
    RestDay,
    /// 法定节假日（`is_workday = false`）。
    StatutoryHoliday,
    /// 调休放假（`is_workday = false`；周末调成放假）。
    MakeupHoliday,
    /// 自定义休息日（`is_workday = false`）。⚠️ **写侧必须带 `name`**
    /// （缺失 → `21907 BIZ_HR_CALENDAR_LEAVE_NAME_REQUIRED`）。
    CustomLeave,
}

impl DayType {
    /// 全部 6 档（按 wire 值升序；`model` 与 `service` 的表驱动单测共用它）。
    pub const ALL: [DayType; 6] = [
        DayType::Workday,
        DayType::MakeupWorkday,
        DayType::RestDay,
        DayType::StatutoryHoliday,
        DayType::MakeupHoliday,
        DayType::CustomLeave,
    ];

    /// DB / wire 字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            DayType::Workday => "WORKDAY",
            DayType::MakeupWorkday => "MAKEUP_WORKDAY",
            DayType::RestDay => "REST_DAY",
            DayType::StatutoryHoliday => "STATUTORY_HOLIDAY",
            DayType::MakeupHoliday => "MAKEUP_HOLIDAY",
            DayType::CustomLeave => "CUSTOM_LEAVE",
        }
    }

    /// 中文标签（`service::DayType::label` 的唯一真源；前端不需要，错误 message 用）。
    pub fn label(self) -> &'static str {
        match self {
            DayType::Workday => "工作日",
            DayType::MakeupWorkday => "调休上班",
            DayType::RestDay => "正常周末",
            DayType::StatutoryHoliday => "法定节假日",
            DayType::MakeupHoliday => "调休放假",
            DayType::CustomLeave => "自定义休息日",
        }
    }

    /// 是否上班日。**这是考勤取数的唯一判据**（`WORKDAY` ∪ `MAKEUP_WORKDAY`）。
    pub fn is_workday(self) -> bool {
        matches!(self, DayType::Workday | DayType::MakeupWorkday)
    }

    /// 解析 wire / DB 字面量；未知值 → `21905 BIZ_HR_DAY_TYPE_INVALID`。
    ///
    /// ⚠️ **不做 trim、不做大写折叠** —— 唯一的合法输入就是 `as_str()` 那 6 个字面量。
    /// 折叠会让「本该在写侧就拒的错值」混进来（库里出现 `workday` 这种行时，
    /// 折叠会把它静默当成合法，暴露成脏数据而不是错误）。
    pub fn parse(s: &str) -> Result<DayType, AppError> {
        DayType::ALL
            .into_iter()
            .find(|d| d.as_str() == s)
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_HR_DAY_TYPE_INVALID,
                    format!("未知日型 `{s}`；合法值：{}", DAY_TYPE_STRINGS.join(" / ")),
                )
            })
    }
}

/// 6 档 wire 字面量（`parse` 的错误 message 与表驱动单测共用）。
pub const DAY_TYPE_STRINGS: [&str; 6] = [
    "WORKDAY",
    "MAKEUP_WORKDAY",
    "REST_DAY",
    "STATUTORY_HOLIDAY",
    "MAKEUP_HOLIDAY",
    "CUSTOM_LEAVE",
];

impl Serialize for DayType {
    /// 出参里的日型**逐字**就是 DB 那一列的字面量（委托给 [`DayType::as_str`]，
    /// 见本枚举上的 doc）。
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DayType {
    /// 反序列化委托给 [`DayType::parse`]（同一真源）。
    ///
    /// ⚠️ **本域的入参 DTO 不直接用这个实现**（它们的 `day_type` 字段是 `String`）——
    /// 那样「未知日型」会落进 axum `Json` 提取器的 **422 纯文本**、拿不到业务信封里的
    /// `21905`。DTO 收 `String` + service 调 `parse` 才是 `21905` 那一档。
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        DayType::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// 按星期推导的兜底日型：周六 / 周日 → `REST_DAY`，其余 → `WORKDAY`。
///
/// ⚠️ 这是**推导规则的最后一级**（全局日历行与班次覆盖行都缺那一行时才轮到它），
/// 见本文件模块 doc 的推导块。
pub fn derive_from_weekday(d: NaiveDate) -> DayType {
    match d.weekday() {
        chrono::Weekday::Sat | chrono::Weekday::Sun => DayType::RestDay,
        _ => DayType::Workday,
    }
}

// ============================================================================
// ShiftSegment —— 时段值对象
// ============================================================================

/// 一个时段（绝对时钟轴）。
///
/// 落库形态：`t_hr_shift.segments` jsonb 数组里的一项
/// `{"seq":1,"start":"22:00","end":"26:00"}`。
///
/// ⚠️ **`seq` 只是展示 / 稳定排序用**，不参与校验（校验按 `start` 升序，见
/// `service::validate_segments` 的 V4）。入参的 `seq` 会被 service 按升序重排后
/// 规范化写库，这样「改了顺序」不会让整个 jsonb 值无谓地变。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShiftSegment {
    /// 段序号（1 起）。写入前由 service 按 `start` 升序重排规范化。
    pub seq: u8,
    /// 起点（`"HH:MM"`，`HH ∈ 0..48`，自 `cal_date 00:00` 起算的绝对偏移）。
    pub start: String,
    /// 终点（同上；`> "24:00"` 表示跨天）。
    pub end: String,
}

impl ShiftSegment {
    /// 便捷构造（单测与 service 内部规范化共用）。
    pub fn new(seq: u8, start: &str, end: &str) -> Self {
        Self {
            seq,
            start: start.to_string(),
            end: end.to_string(),
        }
    }
}

// ============================================================================
// 表行投影（`T*` = 域 owned 表的全列 `FromRow`）
// ============================================================================

/// `t_hr_calendar_day` 全列投影（全局日历**显式行**，只存例外）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TCalendarDayRow {
    pub id: i64,
    pub cal_date: NaiveDate,
    /// `varchar(24)` 原样；**不解成 `DayType`** —— 库里若出现未知字面量，
    /// 由 service 在渲染期降级成 `21905`，而不是让整行解码失败。
    pub day_type: String,
    /// 节假日名 / 休息日说明；`CUSTOM_LEAVE` 时必填（写侧校验 21907）。
    pub name: Option<String>,
    pub remark: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_hr_shift` 全列投影（班次）。
///
/// ⚠️ `segments` 解成 **`serde_json::Value` 而非 `Vec<ShiftSegment>`**：
/// 本仓走运行时 `query_as` 路线（见 `repo` 模块 doc），解成 `Value` 后由
/// `service::parse_segments_strict` 统一 `serde_json::from_value`，解析失败可降级成
/// 明确的 `21903` 而不是让解码在 repo 层炸成 500。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TShiftRow {
    pub id: i64,
    pub version: i32,
    pub code: String,
    pub name: String,
    pub color: Option<String>,
    /// 时段数组（jsonb 原值；解析见上方 doc）。
    pub segments: serde_json::Value,
    pub is_active: bool,
    pub sort_order: i32,
    pub remark: Option<String>,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_hr_shift_calendar` 全列投影（班次的单日日历覆盖行）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TShiftCalendarRow {
    pub id: i64,
    pub version: i32,
    /// 所属班次（无物理外键；存在性由 service 校验 → 21901）。
    pub shift_id: i64,
    pub cal_date: NaiveDate,
    /// 覆盖后的日型（原样字符串，理由同 [`TCalendarDayRow::day_type`]）。
    pub day_type: String,
    pub remark: Option<String>,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 6 档枚举的 wire 字面量必须**逐字**等于 `DAY_TYPE_STRINGS`（顺序也同）——
    /// 前端与 DB 都按这套字面量写死，漂了就是全线对不上。
    #[test]
    fn day_type_strings_match_constant() {
        let from_enum: Vec<&str> = DayType::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(from_enum, DAY_TYPE_STRINGS.to_vec());
    }

    /// `parse` 覆盖 6 档全通 + 未知值返 `21905`（HTTP 400）。
    #[test]
    fn day_type_parse_round_trips_and_rejects_unknown() {
        for d in DayType::ALL {
            assert_eq!(DayType::parse(d.as_str()).unwrap(), d);
        }
        for bad in ["", "workday", "WORKDAY ", "HOLIDAY", "自定义"] {
            let err = DayType::parse(bad).unwrap_err();
            assert_eq!(err.code(), code::BIZ_HR_DAY_TYPE_INVALID);
            assert_eq!(err.http_status(), axum::http::StatusCode::BAD_REQUEST);
        }
    }

    /// `is_workday` 只认 `WORKDAY` + `MAKEUP_WORKDAY` 两档。
    #[test]
    fn only_workday_and_makeup_workday_are_workdays() {
        let on: Vec<&str> = DayType::ALL
            .iter()
            .filter(|d| d.is_workday())
            .map(|d| d.as_str())
            .collect();
        assert_eq!(on, vec!["WORKDAY", "MAKEUP_WORKDAY"]);
    }

    /// 星期推导：周六 / 周日 → `REST_DAY`，周一至周五 → `WORKDAY`。
    /// 2026-10-19 是周一，故基准日用它。
    #[test]
    fn weekday_derivation_maps_sat_sun_to_rest_day() {
        let mon = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        assert_eq!(derive_from_weekday(mon), DayType::Workday);
        for offset in 0..7i64 {
            let d = mon + chrono::Duration::days(offset);
            let expect =
                if d.weekday() == chrono::Weekday::Sat || d.weekday() == chrono::Weekday::Sun {
                    DayType::RestDay
                } else {
                    DayType::Workday
                };
            assert_eq!(derive_from_weekday(d), expect, "{}", d);
        }
    }

    /// ⚠️ **出参里的日型必须逐字等于 DB 字面量**（`REST_DAY` 而不是变体名 `RestDay`）。
    ///
    /// 钉这条是因为 `#[derive(Serialize)]` 的默认行为恰好产出变体名，而 wire 上契约
    /// 是 DB 字面量 —— 两者不一致时前端按 `REST_DAY` 匹配全部落空、服务端日志却全绿。
    #[test]
    fn day_type_serializes_to_the_db_literal_not_the_variant_name() {
        for d in DayType::ALL {
            let v = serde_json::to_value(d).unwrap();
            assert_eq!(
                v,
                serde_json::Value::String(d.as_str().to_string()),
                "{:?} 的 wire 值必须逐字等于 as_str()",
                d
            );
            let back: DayType = serde_json::from_value(v).unwrap();
            assert_eq!(back, d);
        }
    }

    /// 段序列的 serde 形状就是落库的 jsonb 项：`{"seq":1,"start":"22:00","end":"26:00"}`。
    #[test]
    fn shift_segment_serializes_to_jsonb_item_shape() {
        let seg = ShiftSegment::new(1, "22:00", "26:00");
        let v = serde_json::to_value(&seg).unwrap();
        assert_eq!(v["seq"], 1);
        assert_eq!(v["start"], "22:00");
        assert_eq!(v["end"], "26:00");
        let back: ShiftSegment = serde_json::from_value(v).unwrap();
        assert_eq!(back, seg);
    }
}
