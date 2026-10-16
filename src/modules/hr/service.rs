//! `hr`（人事排班）域业务逻辑（**2026-10-19 新增**）：时段校验 V1–V6 +
//! 10 条端点的用例编排。
//!
//! 自由函数（范式同 `serial::service` / `wecom::service`），签名收 `&mut PgConnection`
//! —— **service 不知事务**，事务边界在 handler（CLAUDE.md「必须遵守的架构约定」第 1 条）。
//!
//! ## 权限守卫判在**第一行**（每个用例的第一句）
//!
//! `current.require_permission(PERM_VIEW)` / `require_permission(PERM_MANAGE)`。
//! 放在用例开头而不是 handler，是为了让「无权的账号连候选 SELECT 都跑不到」——
//! 本域的读端点会逐日推导并回传整个区间，写端点会跨两张表做批量写，都不该让无权者
//! 拿它做信息探测。代价是无权账号会**先开一次事务**才被拒（写端点的事务在 handler），
//! 但那只是短连接占用，零写入、不落锁。
//!
//! ## ⚠️ 本域**零 WS 广播**（有意取舍）
//!
//! 本域是**配置页**（人事在后台配日历 / 班次），**没有订阅方**：大屏 WS
//! (`/ws/dashboard`) 的增量清单里没有排班数据，仓库当前也没有任何「排班变更」的
//! 消费端。加一条没有订阅者的广播，等于给每个大屏客户端白加一次防抖与重取。
//! 考勤排班表页面将来落地时，应当走它自己的取数路径（HTTP 首取 + 页面级刷新），
//! 而不是给本域硬接一条 WS。

use std::collections::HashMap;

use chrono::NaiveDate;
use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::hr::dto::{
    COL_COLOR, COL_NAME, COL_REMARK, COL_SHIFT_CODE, CalendarDaysDeleteRequest,
    CalendarDaysUpsertRequest, CalendarRangeQuery, CreateShiftRequest, DAY_BOUNDARY_MINUTES,
    DeactivateShiftRequest, MAX_BATCH_ITEMS, MAX_CLOCK_MINUTES, MAX_RANGE_DAYS, MAX_SEGMENTS,
    MAX_SPAN_MINUTES, MIN_SEGMENTS, PERM_MANAGE, PERM_VIEW, ShiftCalendarRangeQuery,
    ShiftDaysDeleteRequest, ShiftDaysUpsertRequest, ShiftListQuery, UpdateShiftRequest,
};
use crate::modules::hr::model::{
    DayType, ShiftSegment, TShiftCalendarRow, TShiftRow, derive_from_weekday,
};
use crate::modules::hr::repo;
use crate::modules::hr::vo::{
    CalendarDayOut, CalendarDaysOut, CalendarDaysWriteOut, ShiftBandOut, ShiftCalendarDayOut,
    ShiftCalendarOut, ShiftDaysWriteOut, ShiftListOut, ShiftOut,
};
use crate::shared::error::{AppError, code};

// ============================================================================
// 时段校验：绝对时钟轴 V1–V6
// ============================================================================

/// 段值的**严格**正则：`HH:MM`，`HH` 一到两位、`MM` 两位且落在 `00..=59`。
///
/// ⚠️ 刻意**不**做 trim / 大写折叠 / 补零：段值是 `t_hr_shift.segments` 的持久化
/// 字面量，唯一合法形态就是库里那一种。`"8:00"` 会被这条正则**放行**（`{1,2}` 匹配一位
/// 小时），但落库前由 [`format_clock`] 规范化成 `"08:00"`；而 `" 08:00"` / `"8:0"` /
/// `"24:60"` 一律拒。
pub const CLOCK_REGEX: &str = r"^([0-9]{1,2}):([0-5]\d)$";

/// 编译一次的段值正则（`parse_clock` 每次调用都命中它 ⇒ 用 `LazyLock` 免重建，
/// 同 `wecom::dto::webhook_url_regex`）。
fn clock_regex() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(CLOCK_REGEX).expect("常量正则必合法"));
    &RE
}

/// 校验通过后的**规范化**结果（顺带带上三个派生列的原料）。
///
/// `segments` 已按 `start` 升序、`seq` 重编为 `1..n` —— 落库的就是这个形态，
/// 于是「客户端传了乱 seq」不会让整列 jsonb 的字节形态无谓漂移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedSegments {
    pub segments: Vec<ShiftSegment>,
    /// `max(end) > "24:00"`
    pub cross_day: bool,
    /// `max(end) − "24:00"`（分钟），不跨天时为 `0`
    pub overflow_minutes: i32,
    /// `Σ(end − start)`（分钟，无折算）
    pub daily_minutes: i32,
}

/// 时段非法（V1–V5 全部归这一个码）。
fn seg_invalid(msg: impl Into<String>) -> AppError {
    AppError::biz(code::BIZ_HR_SHIFT_SEGMENT_INVALID, msg)
}

/// span 超一天（V6 专用码，见 `error.rs` 的 21908 声明）。
fn span_invalid(msg: impl Into<String>) -> AppError {
    AppError::biz(code::BIZ_HR_SHIFT_SPAN_EXCEEDS_DAY, msg)
}

/// 段值 → 自 `cal_date 00:00` 起算的分钟偏移。
///
/// 判据（三步，顺序固定）：
/// 1. 严格正则 [`CLOCK_REGEX`]：`HH:MM`，`HH` 一到两位、`MM ∈ 00..=59`；
/// 2. `hh > 48` 拒（`"49:00"`）；
/// 3. 总分钟 `> 2880` 拒（`"48:30"` —— `hh ≤ 48` 已经过了第 2 步，但 `48:30`
///    仍然越界，必须单独卡）。
pub fn parse_clock(s: &str) -> Result<i32, AppError> {
    let caps = clock_regex().captures(s).ok_or_else(|| {
        seg_invalid(format!(
            "段值 `{s}` 格式非法（应为 HH:MM，小时 0..48、分钟 00..59）"
        ))
    })?;
    let hh: i32 = caps[1]
        .parse()
        .map_err(|_| seg_invalid(format!("段值 `{s}` 的小时不可解析")))?;
    let mm: i32 = caps[2]
        .parse()
        .map_err(|_| seg_invalid(format!("段值 `{s}` 的分钟不可解析")))?;
    if hh > 48 {
        return Err(seg_invalid(format!(
            "段值 `{s}` 越界：小时最大 48（`48:00`）"
        )));
    }
    let total = hh * 60 + mm;
    if total > MAX_CLOCK_MINUTES {
        return Err(seg_invalid(format!(
            "段值 `{s}` 越界：最大 `48:00`（{MAX_CLOCK_MINUTES} 分钟）"
        )));
    }
    Ok(total)
}

/// 把分钟偏移格式化回 `"HH:MM"`（`24` 小时制，`48:00` 就是 48 点整）。
fn format_clock(minutes: i32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// **班次时段校验 V1–V6**（本域最硬的一处口径，返回规范化结果）。
///
/// | # | 规则 | 违规码 |
/// |---|---|---|
/// | V1 | `0 ≤ start`，`end ≤ "48:00"` | 21903 |
/// | V2 | `start < end`（严格，**不承认零长段**） | 21903 |
/// | V3 | `segments[0].start < "24:00"`（首段必须落在当日） | 21903 |
/// | V4 | 按 `start` 升序且不重叠：`start_{i+1} >= end_i` | 21903 |
/// | V5 | `1 ≤ n ≤ 6` | 21903 |
/// | **V6** | **`span = max(end) − min(start) ≤ "24:00"`** | **21908** |
///
/// ## V3 为什么必需：它是「日期归属」的锚
///
/// 段值是 `cal_date 00:00` 起算的**绝对偏移**，「跨天班次的日期归属 = 工作开始时间
/// 所在日」由数据结构本身保证。若首段起点就在 `24:00` 之后（如 `25:00` 起），那这一班
/// 的**开始时间**落在次日 ⇒ `cal_date` 不再是它的归属日，出参里的
/// `cal_date` / `is_override` / 日历写端点全部失去意义。V3 把这种形态在写侧就拒掉。
///
/// ## ⚠️ V6 是本设计最关键的一刀，**不许放宽**
///
/// **跨日重叠 ⟺ `span > 24h`**。卡死 V6 之后：
/// * 相邻两日（今天 22:00→次日 02:00、明天 22:00→后天 02:00）、隔日的班次实例在
///   时间轴上**结构性地不可能重叠** ⇒ 日历写端点**零重叠检查**（它们只需要「同一天
///   只能有一个班次实例」这条同日语义，那由 `t_hr_shift_calendar` 的
///   `(shift_id, cal_date)` 唯一索引守着）；
/// * 同时它也是「一个班次实例最长 24 小时」的劳动法底线。
///
/// 放宽 V6 的代价不是「多一种排班形态」，而是**日历写端点必须新增一条重叠检查**，
/// 而那条检查要跨 366 天 × N 个班次做区间求交 —— 一次请求 O(days × shifts)，且
/// 与并发插入之间存在 TOCTOU 窗口。V6 是拿「排班形态的表达力」换「日历写端点的
/// 可证明正确」，这个取舍是有意的。
pub fn validate_segments(segments: &[ShiftSegment]) -> Result<NormalizedSegments, AppError> {
    // ── V5：段数 ──
    if segments.len() < MIN_SEGMENTS || segments.len() > MAX_SEGMENTS {
        return Err(seg_invalid(format!(
            "时段段数必须介于 {MIN_SEGMENTS}..={MAX_SEGMENTS}，实得 {}",
            segments.len()
        )));
    }

    // ── 先把每个段值解成分钟（V1 的格式 / 越界部分；解析失败全部 21903）──
    let mut bounds: Vec<(i32, i32)> = Vec::with_capacity(segments.len());
    for seg in segments {
        let start = parse_clock(&seg.start)?;
        let end = parse_clock(&seg.end)?;
        bounds.push((start, end));
    }

    // ── V2：start < end（严格，不承认零长段）──
    for (i, (start, end)) in bounds.iter().enumerate() {
        if start >= end {
            return Err(seg_invalid(format!(
                "第 {} 段 start({}) 必须严格小于 end({})",
                i + 1,
                format_clock(*start),
                format_clock(*end)
            )));
        }
    }

    // ── V4a：按 start 升序（**乱序是错，不是「顺手排一下」**）──
    for i in 1..bounds.len() {
        if bounds[i].0 <= bounds[i - 1].0 {
            return Err(seg_invalid(format!(
                "时段必须按 start 升序：第 {} 段 start({}) 不大于第 {} 段 start({})",
                i + 1,
                format_clock(bounds[i].0),
                i,
                format_clock(bounds[i - 1].0)
            )));
        }
    }

    // ── V4b：不重叠（`start_{i+1} >= end_i`，相邻段可以首尾相接）──
    for i in 1..bounds.len() {
        if bounds[i].0 < bounds[i - 1].1 {
            return Err(seg_invalid(format!(
                "第 {} 段 start({}) 与第 {} 段 end({}) 重叠",
                i + 1,
                format_clock(bounds[i].0),
                i,
                format_clock(bounds[i - 1].1)
            )));
        }
    }

    // ── V3：首段起点必须落在当日 ──
    let first_start = bounds[0].0;
    if first_start >= DAY_BOUNDARY_MINUTES {
        return Err(seg_invalid(format!(
            "首段起点({}) 必须早于 `24:00`，否则工作开始时间不在 cal_date 当日、日期归属无从谈起",
            format_clock(first_start)
        )));
    }

    // ── V6：span ≤ 24:00（本域的结构性护栏，见上方 doc）──
    let min_start = first_start;
    let max_end = bounds.iter().map(|(_, end)| *end).max().unwrap_or(0);
    let span = max_end - min_start;
    if span > MAX_SPAN_MINUTES {
        return Err(span_invalid(format!(
            "时段总跨度 {}（{} → {}）超过 24:00；跨日重叠 ⟺ span > 24h，\
             卡死这条才能让日历写端点零重叠检查",
            format_clock(span),
            format_clock(min_start),
            format_clock(max_end)
        )));
    }

    // ── 规范化：seq 重编为 1..n，段值补零成 `"HH:MM"` ──
    let normalized = bounds
        .iter()
        .enumerate()
        .map(|(i, (start, end))| {
            ShiftSegment::new(i as u8 + 1, &format_clock(*start), &format_clock(*end))
        })
        .collect::<Vec<_>>();

    Ok(NormalizedSegments {
        daily_minutes: bounds.iter().map(|(s, e)| e - s).sum(),
        cross_day: max_end > DAY_BOUNDARY_MINUTES,
        overflow_minutes: (max_end - DAY_BOUNDARY_MINUTES).max(0),
        segments: normalized,
    })
}

/// 读库路径用：jsonb 原值 → 规范化结果。
///
/// ⚠️ **两处降级都不是 500**：
/// * `serde_json::from_value` 失败（库里那一列被手工改坏）→ `21903`；
/// * 解析成功但**过不了 V1–V6**（CHECK 约束只管「1..6 个元素」，管不住元素内容）→
///   同一条 `validate_segments` 的码。
///
/// 复用校验而不是「读出来就算」：三个派生列（`cross_day` / `overflow_minutes` /
/// `daily_minutes`）必须**永远**是由同一套规则算出来的，否则同一行在不同请求里会
/// 给出不同的派生值。
pub fn parse_segments_strict(v: &serde_json::Value) -> Result<NormalizedSegments, AppError> {
    let raw: Vec<ShiftSegment> = serde_json::from_value(v.clone()).map_err(|e| {
        seg_invalid(format!(
            "班次时段列不是合法的时段数组（jsonb 解析失败：{e}）；请用写端点重建该班次的时段"
        ))
    })?;
    validate_segments(&raw)
}

/// 行 → 出参（含三个派生列；派生失败按上面的口径降级而不是 500）。
fn shift_out(row: &TShiftRow) -> Result<ShiftOut, AppError> {
    let norm = parse_segments_strict(&row.segments)?;
    Ok(ShiftOut {
        id: row.id,
        version: row.version,
        code: row.code.clone(),
        name: row.name.clone(),
        color: row.color.clone(),
        segments: norm.segments,
        is_active: row.is_active,
        sort_order: row.sort_order,
        remark: row.remark.clone(),
        cross_day: norm.cross_day,
        overflow_minutes: norm.overflow_minutes,
        daily_minutes: norm.daily_minutes,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

// ============================================================================
// 区间校验（21904）
// ============================================================================

/// 区间合法性：`from <= to` 且跨度 ≤ [`MAX_RANGE_DAYS`] 天（两端闭）。
///
/// ⚠️ 批量 upsert 的 `{items:[…]}` 走**同一条**判据：`items` 的最小 / 最大 `cal_date`
/// 构成一个隐式区间。不卡它的话，攒一个大 `items` 就能把「一个请求最多 366 天」的
/// 承诺从读端点绕过去 —— 那个上限本来就是防「一个请求灌 10 年」用的。
pub fn validate_range(from: NaiveDate, to: NaiveDate) -> Result<(), AppError> {
    if from > to {
        return Err(AppError::biz(
            code::BIZ_HR_CALENDAR_RANGE_INVALID,
            format!("区间非法：from({from}) 晚于 to({to})"),
        ));
    }
    let days = (to - from).num_days();
    if days > MAX_RANGE_DAYS {
        return Err(AppError::biz(
            code::BIZ_HR_CALENDAR_RANGE_INVALID,
            format!("区间跨度 {days} 天超过上限 {MAX_RANGE_DAYS} 天"),
        ));
    }
    Ok(())
}

/// 一批 `cal_date` 的隐式区间校验（空批 / 条数超限 / 跨度超限，一律 `21904`）。
///
/// ⚠️ 三个判据缺一不可：
/// * **空批** → `21904`（「没说要写什么」不是合法请求）；
/// * **条数 > [`MAX_BATCH_ITEMS`]** → `21904`：跨度判据只看「最小 ~ 最大日期」，
///   一万个**全同日期**的 `items` 能把跨度绕过去（跨度恒为 0）却仍是一次巨大 UNNEST；
/// * **跨度 > [`MAX_RANGE_DAYS`]** → `21904`。
fn validate_date_batch(dates: &[NaiveDate]) -> Result<(), AppError> {
    if dates.is_empty() {
        return Err(AppError::biz(
            code::BIZ_HR_CALENDAR_RANGE_INVALID,
            "items 为空：没有要写入的日期",
        ));
    }
    if dates.len() > MAX_BATCH_ITEMS {
        return Err(AppError::biz(
            code::BIZ_HR_CALENDAR_RANGE_INVALID,
            format!("一批 {} 个日期超过上限 {MAX_BATCH_ITEMS} 条", dates.len()),
        ));
    }
    let min = dates.iter().copied().min().expect("非空");
    let max = dates.iter().copied().max().expect("非空");
    validate_range(min, max)
}

/// 按 `cal_date` 去重一批判次（**后写覆盖先写**）。
///
/// ## 为什么必须去重（2026-10-23）
///
/// 批量 upsert 的 SQL 是 `INSERT … SELECT … FROM UNNEST(…) ON CONFLICT (cal_date)
/// WHERE deleted_at IS NULL DO UPDATE`。同一条命令里影响同一行两次时 PG 抛
/// `ON CONFLICT DO UPDATE command cannot affect row a second time`
/// （SQLSTATE `21000`）—— 它既不是 `23505`（`is_unique_violation` 判不到）也不在任何
/// 显式档 ⇒ 落 `AppError::Database` → `50001` / **HTTP 500**。而「同批里出现同一个
/// `cal_date`」的门槛是零：前端一次范围拖选 + 一次补写就能产生它。
///
/// **后写覆盖先写**（而不是报错）与 [`into_day_map`] 的「脏数据后写覆盖先写」同口径：
/// 同一批里同一天只能有一个最终口径，后出现的那条就是调用方的最新意图。
///
/// 保序：结果按**第一次出现的位置**排列，让「`affected` 与入参顺序对得上」这条可预期
/// 的性质留在（虽然它不参与任何语义判定）。
fn dedup_by_date<T: Clone>(items: &[T], date_of: impl Fn(&T) -> NaiveDate) -> Vec<T> {
    let mut kept: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        let d = date_of(item);
        match kept.iter_mut().find(|k| date_of(k) == d) {
            Some(slot) => *slot = item.clone(),
            None => kept.push(item.clone()),
        }
    }
    kept
}

/// 删行请求的两种形态归一：`{dates:[…]}` 或 `{from,to}`（必恰好命中一种）。
#[derive(Debug)]
enum DeleteTarget {
    Dates(Vec<NaiveDate>),
    Range(NaiveDate, NaiveDate),
}

/// 把 `{dates | from+to}` 归一成 [`DeleteTarget`]；两种都没给 / 都给 / `dates` 为空
/// 一律 `21904`（而不是「删 0 行」的假成功）。
fn resolve_delete_target(
    dates: Option<Vec<NaiveDate>>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<DeleteTarget, AppError> {
    match (dates, from, to) {
        (Some(ds), None, None) => {
            if ds.is_empty() {
                return Err(AppError::biz(
                    code::BIZ_HR_CALENDAR_RANGE_INVALID,
                    "dates 为空数组：等于没说要删什么",
                ));
            }
            validate_date_batch(&ds)?;
            Ok(DeleteTarget::Dates(ds))
        }
        (None, Some(f), Some(t)) => {
            validate_range(f, t)?;
            Ok(DeleteTarget::Range(f, t))
        }
        // ⚠️ 其余全部形态（都省 / 只给一半 / 都给）一律 21904。用 catch-all 而不是
        // 逐个列举：`Option<Vec<_>>` 的组合有 3×2×2 = 12 种，逐个列必然漏一两个。
        _ => Err(AppError::biz(
            code::BIZ_HR_CALENDAR_RANGE_INVALID,
            "删行请求必须二选一：{dates:[…]} 或 {from,to}（不能都省、不能只给一半、不能都给）",
        )),
    }
}

// ============================================================================
// 字段值守卫（21909）
// ============================================================================

/// 字段值非法（`21909`，HTTP 400）：必填字段为空白，或某字段超出 `varchar` 列宽。
fn field_invalid(msg: impl Into<String>) -> AppError {
    AppError::biz(code::BIZ_HR_FIELD_INVALID, msg)
}

/// 列宽守卫：`v` 的**字符数**超过 `max` ⇒ `21909`。
///
/// ⚠️ **按字符数、不按字节数**：PG 的 `varchar(n)` 计的是**字符**，
/// `varchar(30)` 放得下 30 个汉字（90 字节）。按 `len()`（字节）判会把
/// 10 个汉字的 `name` 误拒成 400 —— 那比 500 更坏，因为它挡掉合法输入。
fn check_len(field: &str, v: &str, max: usize) -> Result<(), AppError> {
    let n = v.chars().count();
    if n > max {
        return Err(field_invalid(format!(
            "{field} 长度 {n} 超过上限 {max} 个字符"
        )));
    }
    Ok(())
}

/// 必填文本：先 `trim()`，再判空 —— 空串 / 纯空白一律 `21909`。
///
/// ⚠️ 为什么卡：`code` 空串会**占住** `uk_t_hr_shift_code` 的 partial unique
/// （一旦有人建出 `code = ''`，后续所有人想用空 code 都撞 `21902`，而这条空班次
/// 本身没有任何业务含义）；`name` 空串则让排班表上出现一行没名字的班次。
/// 两者都是「入参非法」，不是「状态冲突」。
fn require_text(field: &str, v: &str) -> Result<String, AppError> {
    let t = v.trim();
    if t.is_empty() {
        return Err(field_invalid(format!("{field} 不能为空或全是空白")));
    }
    Ok(t.to_string())
}

// ============================================================================
// 日型推导的唯一真源（端点 1 / 端点 8 共用，2026-10-19 抽成共用函数）
// ============================================================================

/// 三级推导（**覆盖行 > 全局行 > 按星期兜底**）的**唯一实现**。
///
/// 返回 `(global_day_type, effective_day_type, is_override)`。
///
/// ## ⚠️ 为什么两处**必须**共用这一个函数（而不是各写一遍）
///
/// 端点 1 的 `shift_bands`（每日 × 每班次）与端点 8 的 `items`（每日 × 单班次）要回答
/// **同一个问题**：「班次 `shift_id` 在 `date` 这天上不上班」。两处各判一次，哪怕
/// 判据一字不差也**必然会漂移** —— 一处改了、另一处没改时，两个端点对同一天给出
/// 不同的 `is_workday`，而 `is_workday` 是**考勤取数的唯一判据**。派生规则一旦分叉，
/// 考勤算出的工时就会与排班表显示的对不上，且没有任何报错。
///
/// 前端同理**不得**自己重算（`docs/api/hr.md` §7 第 1 / 2 条）：`is_workday` 一律读
/// 出参。
///
/// `globals` 收的是**原始字符串**（`HashMap<NaiveDate, String>`）：库里若出现未知
/// 日型字面量，由这里的 [`DayType::parse`] 降级成 `21905`，而不是让整行解码失败
/// （与 `model::TCalendarDayRow::day_type` 的口径同源）。
///
/// ⚠️ **失败半径**：端点 1 要对**每个活跃班次 × 区间内每一天**各调一次本函数 ⇒ **任一**
/// 活跃班次在**任一天**的覆盖行（或全局行）字面量不合法，**整个请求**就是 `21905` /
/// HTTP 400 —— 半径是「全局日历 × 整个区间」，不是「单个班次」。端点 8 的同款降级
/// 半径只有单班次。这是共用同一个函数的**代价**，有意接受：色带本身就是「所有班次 ×
/// 整天」的笛卡尔积，出参形状已经要求这一点；而把两处推导拆开各判一次，漂移是无声的
/// （见上方「为什么必须共用」）。
fn derive_effective(
    globals: &HashMap<NaiveDate, String>,
    overrides: &HashMap<(i64, NaiveDate), String>,
    shift_id: i64,
    date: NaiveDate,
) -> Result<(DayType, DayType, bool), AppError> {
    // 第 2 级：全局日历行（无显式行 ⇒ 按星期兜底）
    let global_day_type = match globals.get(&date) {
        Some(raw) => DayType::parse(raw)?,
        None => derive_from_weekday(date),
    };
    // 第 1 级：班次覆盖行（键含 shift_id ⇒ 同一个 map 可同时供多班次共用）
    let (effective_day_type, is_override) = match overrides.get(&(shift_id, date)) {
        Some(raw) => (DayType::parse(raw)?, true),
        None => (global_day_type, false),
    };
    Ok((global_day_type, effective_day_type, is_override))
}

// ============================================================================
// 端点 1：全局日历区间读
// ============================================================================

/// `GET /calendar/days?from&to` —— 全局日历区间读（**逐日一行，一天都不省**）。
///
/// 返回的每一天要么来自显式行（`derived = false`），要么按星期推导补齐
/// （`derived = true`）；每行另带 `shift_bands`（逐班次色带，2026-10-19 新增）。
///
/// ## `shift_bands` 的取数与性能（2026-10-19）
///
/// SQL 总共**仍是 3 条**：`list_calendar_days`（全局显式行）+ `list_shifts(.., false)`
/// （活跃班次）+ `list_shift_overrides_in_range`（区间内**全部**班次的覆盖行）。
/// 「每一天 × 每个活跃班次」的推导是**纯内存**的 `HashMap` 查表 +
/// [`derive_from_weekday`]，**不是** N+1 查询 —— 若按班次循环
/// `repo::list_shift_days` 就是 N+1 次，那才是这里刻意避开的。
///
/// ⚠️ **已知取舍（登记在 `docs/api/hr.md` §8.3）**：响应体大小是
/// **区间天数 × 活跃班次数**。跨度上限 366 天 ⇒ 两端闭**最多 367 天**（§5.2）配
/// 20 个活跃班次 ⇒ 最多 **7340 条 band**，出参会明显变大。**有意不改区间上限** ——
/// 上限防的是「一个请求灌 10 年」；色带是前端渲染排班表的必需数据，砍掉它等于让前端
/// 回到 N+1 次请求。
pub async fn list_calendar_days(
    conn: &mut PgConnection,
    current: &CurrentUser,
    query: &CalendarRangeQuery,
) -> Result<CalendarDaysOut, AppError> {
    current.require_permission(PERM_VIEW)?;
    validate_range(query.from, query.to)?;

    let rows = repo::list_calendar_days(conn, query.from, query.to).await?;
    // 色带的两个原料：区间内全部班次的覆盖行 + 全部**活跃**班次（各一次读，见上方性能注）。
    let override_rows = repo::list_shift_overrides_in_range(conn, query.from, query.to).await?;
    let mut shifts = repo::list_shifts(conn, false).await?;
    // ⚠️ **必须在这里重排**：`repo::list_shifts` 的兜底键是 `id ASC`，而契约要求
    // `sort_order ASC` → `code ASC`（`code` 在未软删行内唯一 ⇒ 全序、顺序稳定）。
    // 前端按数组顺序切色带段，顺序漂了就画错。
    //
    // ⚠️ 重排也意味着**色带顺序与端点 4 `GET /shifts` 不保证一致**（那边 `sort_order`
    // 打平时按 `id` 兜底）⇒ 前端**必须按 `shift_id` 配对**、不得按下标配对，否则色带
    // 会串到别的班次上且无任何报错。登记在 `docs/api/hr.md` §4.1.1 第 1 条。
    shifts.sort_by(|a, b| {
        a.sort_order
            .cmp(&b.sort_order)
            .then_with(|| a.code.cmp(&b.code))
    });

    let globals = into_day_map(rows.iter().map(|r| (r.cal_date, r.day_type.clone())));
    let overrides = into_shift_day_map(override_rows.into_iter());

    let mut items = Vec::with_capacity((query.to - query.from).num_days() as usize + 1);
    // ⚠️ 走「一个游标 + 一个下标」的线性推进，而不是 `HashMap`：`rows` 已按 `cal_date`
    // 升序返回，两条有序序列求差集在 O(n) 里完成，不必建索引表。
    let mut it = rows.into_iter();
    let mut next_row = it.next();
    let mut cursor = query.from;
    while cursor <= query.to {
        // 逐班次色带（无活跃班次时是空数组，不是 `null` —— 前端直接遍历即可）。
        let mut shift_bands = Vec::with_capacity(shifts.len());
        for shift in &shifts {
            let (_, effective_day_type, is_override) =
                derive_effective(&globals, &overrides, shift.id, cursor)?;
            shift_bands.push(ShiftBandOut {
                shift_id: shift.id,
                effective_day_type,
                is_workday: effective_day_type.is_workday(),
                is_override,
            });
        }
        match next_row.as_ref().filter(|r| r.cal_date == cursor) {
            Some(_) => {
                let row = next_row.take().expect("刚 peek 到必然还挂着");
                items.push(CalendarDayOut {
                    cal_date: row.cal_date,
                    day_type: DayType::parse(&row.day_type)?,
                    name: row.name,
                    derived: false,
                    shift_bands,
                });
                next_row = it.next();
            }
            None => items.push(CalendarDayOut {
                cal_date: cursor,
                day_type: derive_from_weekday(cursor),
                name: None,
                derived: true,
                shift_bands,
            }),
        }
        cursor += chrono::Duration::days(1);
    }
    Ok(CalendarDaysOut { items })
}

// ============================================================================
// 端点 2：全局日历批量 upsert
// ============================================================================

/// `POST /calendar/days` —— 批量 upsert 全局日历显式行（**只存例外**）。
pub async fn upsert_calendar_days(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    current: &CurrentUser,
    req: &CalendarDaysUpsertRequest,
) -> Result<CalendarDaysWriteOut, AppError> {
    current.require_permission(PERM_MANAGE)?;

    // 先校验（空批 / 条数 / 跨度，判在**入参原始条数**上），再去重。
    let dates: Vec<NaiveDate> = req.items.iter().map(|i| i.cal_date).collect();
    validate_date_batch(&dates)?;

    // ⚠️ **必须去重**：PG 的
    // `ON CONFLICT DO UPDATE command cannot affect row a second time`（SQLSTATE 21000）
    // 落不进任何显式档 ⇒ 同批里出现同一个 `cal_date` 就是 HTTP 500。而它的门槛是零：
    // 前端一次范围拖选 + 一次补写就能产生它。
    let items = dedup_by_date(&req.items, |i| i.cal_date);

    // 逐条校验（day_type 合法性 / CUSTOM_LEAVE 必须带 name / 列宽），**全部过了再落库**
    // —— 批写是「要么这批全过、要么一行不进」的语义（handler 的事务边界）。
    let mut rows = Vec::with_capacity(items.len());
    for item in &items {
        let dt = DayType::parse(&item.day_type)?;
        if dt == DayType::CustomLeave && item.name.as_deref().unwrap_or("").trim().is_empty() {
            return Err(AppError::biz(
                code::BIZ_HR_CALENDAR_LEAVE_NAME_REQUIRED,
                format!(
                    "{} 是自定义休息日，必须填 name（排班表要显示它）",
                    item.cal_date
                ),
            ));
        }
        // 超长 ⇒ PG `22001` ⇒ 500；必须在入参层拒成 21909。`name` 是节假日名 / 休息日
        // 说明（人事随手打字），`remark` 是自由文本 —— 两列都要卡。
        if let Some(name) = &item.name {
            check_len("name", name, COL_NAME)?;
        }
        if let Some(remark) = &item.remark {
            check_len("remark", remark, COL_REMARK)?;
        }
        rows.push(repo::NewCalendarDay {
            id: snowflake.next_id(),
            cal_date: item.cal_date,
            day_type: dt.as_str().to_string(),
            name: item.name.clone(),
            remark: item.remark.clone(),
        });
    }

    let affected = repo::upsert_calendar_days(conn, &rows, current.id, now_naive()).await?;
    Ok(CalendarDaysWriteOut { affected })
}

// ============================================================================
// 端点 3：全局日历批量删行
// ============================================================================

/// `POST /calendar/days/delete` —— 删掉显式行，那几天**回退到按星期推导**。
///
/// ⚠️ `affected = 0` 是**合法成功**：那几天本来就没有显式行。
pub async fn delete_calendar_days(
    conn: &mut PgConnection,
    current: &CurrentUser,
    req: &CalendarDaysDeleteRequest,
) -> Result<CalendarDaysWriteOut, AppError> {
    current.require_permission(PERM_MANAGE)?;
    let target = resolve_delete_target(req.dates.clone(), req.from, req.to)?;
    let affected = match target {
        DeleteTarget::Dates(ds) => {
            repo::soft_delete_calendar_days_by_dates(conn, &ds, current.id, now_naive()).await?
        }
        DeleteTarget::Range(f, t) => {
            repo::soft_delete_calendar_days_by_range(conn, f, t, current.id, now_naive()).await?
        }
    };
    Ok(CalendarDaysWriteOut { affected })
}

// ============================================================================
// 端点 4：班次列表
// ============================================================================

/// `GET /shifts?include_inactive` —— 班次列表（含三个派生列）。
pub async fn list_shifts(
    conn: &mut PgConnection,
    current: &CurrentUser,
    query: &ShiftListQuery,
) -> Result<ShiftListOut, AppError> {
    current.require_permission(PERM_VIEW)?;
    let rows = repo::list_shifts(conn, query.include_inactive).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        items.push(shift_out(row)?);
    }
    Ok(ShiftListOut { items })
}

// ============================================================================
// 端点 5：建班次
// ============================================================================

/// `POST /shifts` —— 建班次（`version = 0`、`is_active = true`）。
pub async fn create_shift(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    current: &CurrentUser,
    req: &CreateShiftRequest,
) -> Result<ShiftOut, AppError> {
    current.require_permission(PERM_MANAGE)?;
    let norm = validate_segments(&req.segments)?;

    // 字段值守卫（21909）：必填项 trim 后判空、可空项按**字符数**判列宽。
    // ⚠️ 顺序在 `code_exists` 之前：空 code 预检会撞上「已存在的空 code」而误报 21902，
    // 超长 code 预检则会拿一个注定写不进去的值去查库。
    let code = require_text("code", &req.code)?;
    check_len("code", &code, COL_SHIFT_CODE)?;
    let name = require_text("name", &req.name)?;
    check_len("name", &name, COL_NAME)?;
    if let Some(color) = &req.color {
        check_len("color", color, COL_COLOR)?;
    }
    if let Some(remark) = &req.remark {
        check_len("remark", remark, COL_REMARK)?;
    }

    // 应用层预检（friendly message）+ DB partial unique（并发兜底）。
    // ⚠️ 两条都映射成**同一个** `21902`：调用方不该区分「我查出来的」与「撞并发撞出来的」。
    if repo::code_exists(conn, &code, None).await? {
        return Err(code_dup_error(&code));
    }
    let id = snowflake.next_id();
    let segments_json: Vec<serde_json::Value> = norm
        .segments
        .iter()
        .map(|s| serde_json::to_value(s).expect("ShiftSegment 必可序列化"))
        .collect();
    let new = repo::NewShift {
        id,
        code,
        name,
        color: req.color.clone(),
        segments: segments_json,
        sort_order: req.sort_order.unwrap_or(0),
        remark: req.remark.clone(),
        operator: current.id,
        now: now_naive(),
    };
    repo::insert_shift(conn, &new).await.map_err(|e| {
        if crate::shared::error::is_unique_violation(&e) {
            code_dup_error(&new.code)
        } else {
            AppError::Database(e)
        }
    })?;

    let row = repo::get_shift(conn, id)
        .await?
        .ok_or_else(|| AppError::internal("班次刚建好就读不回来"))?;
    shift_out(&row)
}

/// code 重复（`21902`，HTTP **409** —— ⚠️ 不是 `40901`，后者只表达 OCC）。
fn code_dup_error(code: &str) -> AppError {
    AppError::biz(
        code::BIZ_HR_SHIFT_CODE_DUPLICATED,
        format!("班次编码 `{code}` 已存在"),
    )
}

// ============================================================================
// 端点 6：改班次
// ============================================================================

/// `POST /shifts/{id}` —— 改班次（**部分覆盖** + OCC）。
///
/// `segments` 是**单列覆盖**（传了就整列替换，不做段级 merge）；其余字段同理
/// （body 里给了才改）。⚠️ `color` / `remark` 传 `null` 等于「**这次不改**」——
/// 本端点**没有**「显式清空某个可空列」的语义（要清空就传空串）。
pub async fn update_shift(
    conn: &mut PgConnection,
    current: &CurrentUser,
    id: i64,
    req: &UpdateShiftRequest,
) -> Result<ShiftOut, AppError> {
    current.require_permission(PERM_MANAGE)?;

    let row = repo::get_shift(conn, id)
        .await?
        .ok_or_else(shift_not_found)?;
    ensure_writable(&row)?;

    // 时段：给了就重新走一遍 V1–V6（不能只信「它是之前校验过的」—— 客户端可以
    // 直接传一条手工构造的 segments）；没给就沿用行上那份（同样过一遍校验，让三个
    // 派生列与写路径同源）。
    let norm = match &req.segments {
        Some(segs) => validate_segments(segs)?,
        None => parse_segments_strict(&row.segments)?,
    };
    let segments_json: Vec<serde_json::Value> = norm
        .segments
        .iter()
        .map(|s| serde_json::to_value(s).expect("ShiftSegment 必可序列化"))
        .collect();

    let new_code = match &req.code {
        Some(c) => {
            let t = require_text("code", c)?;
            check_len("code", &t, COL_SHIFT_CODE)?;
            t
        }
        None => row.code.clone(),
    };
    let new_name = match &req.name {
        Some(n) => {
            let t = require_text("name", n)?;
            check_len("name", &t, COL_NAME)?;
            t
        }
        None => row.name.clone(),
    };
    if let Some(color) = &req.color {
        check_len("color", color, COL_COLOR)?;
    }
    if let Some(remark) = &req.remark {
        check_len("remark", remark, COL_REMARK)?;
    }
    if new_code != row.code && repo::code_exists(conn, &new_code, Some(id)).await? {
        return Err(code_dup_error(&new_code));
    }

    let patch = repo::ShiftPatch {
        code: new_code,
        name: new_name,
        color: req.color.clone().or_else(|| row.color.clone()),
        segments: segments_json,
        sort_order: req.sort_order.unwrap_or(row.sort_order),
        remark: req.remark.clone().or_else(|| row.remark.clone()),
        operator: current.id,
        now: now_naive(),
    };
    let affected = repo::update_shift(conn, id, req.version, &patch)
        .await
        .map_err(|e| {
            // 改成已占用的 code → 撞 `uk_t_hr_shift_code`（并发兜底，与上面的预检同码）。
            if crate::shared::error::is_unique_violation(&e) {
                code_dup_error(&patch.code)
            } else {
                AppError::Database(e)
            }
        })?;
    if affected == 0 {
        // ⚠️ 0 行有**三种**成因，一律归因成 OCC 会把「已被并发停用」说成
        // 「版本陈旧」，两种的处置完全不同（前者要刷新，后者要重新加载后再提交）。
        return Err(attribute_zero_row_shift(conn, id).await);
    }

    let row = repo::get_shift(conn, id)
        .await?
        .ok_or_else(shift_not_found)?;
    shift_out(&row)
}

/// OCC 冲突（`40901`）。⚠️ **只表达 OCC**，业务唯一键冲突走 `21902`。
fn occ_conflict() -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        "班次已被他人修改，请重新加载后再提交",
    )
}

/// **0 行归因（返回的错误必是 `Err`）**：读一次班次行把成因分清。
///
/// | 库里的现状 | 归因 |
/// |---|---|
/// | 查不到 / 已软删 | `21901`（调用方该换个 id） |
/// | 在但 `is_active = false` | `21906`（并发停用，处置是「刷新后再看」） |
/// | 在、启用中 | `40901` OCC（`version` 陈旧，处置是「重新加载后再提交」） |
///
/// ⚠️ **为什么必须分清后两种**：它们给前端的下一步动作完全不同（刷新 vs 重新加载
/// 后重试），一律归因成 OCC 会让「这个班次已经停用」这件事被说成「你版本旧了」，
/// 而后者会让客户端反复重试一个永远过不去的请求。
async fn attribute_zero_row_shift(conn: &mut PgConnection, id: i64) -> AppError {
    match repo::get_shift(conn, id).await {
        Ok(None) => shift_not_found(),
        Ok(Some(row)) if !row.is_active => inactive_error(&row),
        Ok(Some(_)) => occ_conflict(),
        Err(e) => e,
    }
}

/// **0 行归因（软删类写 SQL）**：`Ok(())` = 班次仍可写，0 行只是「那几天本来就没有行」。
///
/// 与 [`attribute_zero_row_shift`] 的区别就在这个 `Ok` 分支：清覆盖的
/// `affected = 0` 是**合法成功**（见 §4.4），所以「班次可写」时不能报错。
async fn ensure_shift_still_writable(conn: &mut PgConnection, id: i64) -> Result<(), AppError> {
    let row = repo::get_shift(conn, id)
        .await?
        .ok_or_else(shift_not_found)?;
    ensure_writable(&row)
}

/// 班次不存在 / 已软删（`21901`，HTTP 404）。
fn shift_not_found() -> AppError {
    AppError::biz(code::BIZ_HR_SHIFT_NOT_FOUND, "班次不存在或已删除")
}

/// 对已停用班次做写操作（`21906`，HTTP 409）。
fn inactive_error(row: &TShiftRow) -> AppError {
    AppError::biz(
        code::BIZ_HR_SHIFT_INACTIVE,
        format!(
            "班次「{}」（{}）已停用，不能再改 / 写覆盖行 / 清覆盖",
            row.name, row.code
        ),
    )
}

/// 「这个班次现在能不能被写」的唯一判据（停用 ⇒ 一切写操作 409）。
fn ensure_writable(row: &TShiftRow) -> Result<(), AppError> {
    if row.is_active {
        Ok(())
    } else {
        Err(inactive_error(row))
    }
}

// ============================================================================
// 端点 7：停用班次
// ============================================================================

/// `POST /shifts/{id}/deactivate` —— 停用（软下架）。
///
/// ⚠️ **不软删 `t_hr_shift` 行、不级联软删 `t_hr_shift_calendar` 的覆盖行**：
/// 历史考勤还要复现当年那天的口径。重复停用 → `21906`（幂等拒绝）。
pub async fn deactivate_shift(
    conn: &mut PgConnection,
    current: &CurrentUser,
    id: i64,
    req: &DeactivateShiftRequest,
) -> Result<ShiftOut, AppError> {
    current.require_permission(PERM_MANAGE)?;
    let row = repo::get_shift(conn, id)
        .await?
        .ok_or_else(shift_not_found)?;
    ensure_writable(&row)?;

    let affected = repo::deactivate_shift(conn, id, req.version, current.id, now_naive()).await?;
    if affected == 0 {
        // 0 行 = `version` 陈旧 / 行已不在 / **已被并发停用**（SQL 里带 `AND is_active`）。
        // 并发重复停用应当给 `21906` 而不是 `40901`：调用方重试一个已停用的班次
        // 永远过不去，说成 OCC 只会让客户端空转。
        return Err(attribute_zero_row_shift(conn, id).await);
    }
    let row = repo::get_shift(conn, id)
        .await?
        .ok_or_else(shift_not_found)?;
    shift_out(&row)
}

// ============================================================================
// 端点 8：班次区间日历
// ============================================================================

/// `GET /shifts/{id}/calendar?from&to` —— 班次的工作日历区间读。
///
/// 每一天同时回传 `global_day_type`（全局日历说的）与 `effective_day_type`
/// （覆盖行 / 全局行 / 星期推导三级推导的结果）⇒ 前端可以既显示「这天全局是节假日」
/// 又显示「但这个班次那天照常上班」，**不需要自己实现推导**。
///
/// ⚠️ 推导走 [`derive_effective`] —— 与端点 1 的 `shift_bands` **同一条路径**，
/// 改判据时两个端点一起变，不存在「两边各判一次」的漂移。
pub async fn list_shift_calendar(
    conn: &mut PgConnection,
    current: &CurrentUser,
    id: i64,
    query: &ShiftCalendarRangeQuery,
) -> Result<ShiftCalendarOut, AppError> {
    current.require_permission(PERM_VIEW)?;
    validate_range(query.from, query.to)?;
    // 班次不存在 / 已软删 → 404（读端点同样要判，否则会对着一个不存在的班次返回
    // 一整段「全部走推导」的空日历，看起来像「这个班次不排班」）。
    let _ = repo::get_shift(conn, id)
        .await?
        .ok_or_else(shift_not_found)?;

    let global_rows = repo::list_calendar_days(conn, query.from, query.to).await?;
    let override_rows = repo::list_shift_days(conn, id, query.from, query.to).await?;

    let globals = into_day_map(global_rows.into_iter().map(|r| (r.cal_date, r.day_type)));
    let overrides = into_shift_day_map(override_rows.into_iter());

    let mut items = Vec::with_capacity((query.to - query.from).num_days() as usize + 1);
    let mut cursor = query.from;
    while cursor <= query.to {
        let (global_day_type, effective_day_type, is_override) =
            derive_effective(&globals, &overrides, id, cursor)?;
        items.push(ShiftCalendarDayOut {
            cal_date: cursor,
            global_day_type,
            effective_day_type,
            is_workday: effective_day_type.is_workday(),
            is_override,
        });
        cursor += chrono::Duration::days(1);
    }
    Ok(ShiftCalendarOut {
        shift_id: id,
        items,
    })
}

/// `Vec<(NaiveDate, String)>` → `HashMap<NaiveDate, String>`。
///
/// ⚠️ 若同一日期出现两次（partial unique 索引漏网 / 手工改过库），**后写覆盖先写**
/// 而不是报错 —— 读端点不该因为库里有一条脏数据就整个失败（那会把「这张日历打不开」
/// 变成排班系统不可用）。写入侧的唯一索引才是防线。
fn into_day_map(rows: impl Iterator<Item = (NaiveDate, String)>) -> HashMap<NaiveDate, String> {
    rows.collect()
}

/// `t_hr_shift_calendar` 行 → `HashMap<(shift_id, cal_date), String>`（**键含班次**）。
///
/// ⚠️ 键含 `shift_id` 是为了端点 1 与端点 8 能**共用同一个 map 与同一个
/// [`derive_effective`]**：端点 1 一次读回区间内**全部**班次的覆盖行，端点 8 只放一个
/// 班次的行，两边都是这张表的行、按同一种键查即可。
/// 「后写覆盖先写」的脏数据口径同 [`into_day_map`]。
fn into_shift_day_map(
    rows: impl Iterator<Item = TShiftCalendarRow>,
) -> HashMap<(i64, NaiveDate), String> {
    rows.map(|r| ((r.shift_id, r.cal_date), r.day_type))
        .collect()
}

// ============================================================================
// 端点 9：班次覆盖行批量 upsert
// ============================================================================

/// `POST /shifts/{id}/calendar/days` —— 批量 upsert 班次覆盖行。
pub async fn upsert_shift_days(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    current: &CurrentUser,
    id: i64,
    req: &ShiftDaysUpsertRequest,
) -> Result<ShiftDaysWriteOut, AppError> {
    current.require_permission(PERM_MANAGE)?;
    let row = repo::get_shift(conn, id)
        .await?
        .ok_or_else(shift_not_found)?;
    ensure_writable(&row)?;

    // 同端点 2：条数 / 跨度判在原始 `items` 上，判完再去重（同批重复日期 ⇒ 21000 ⇒ 500）。
    let dates: Vec<NaiveDate> = req.items.iter().map(|i| i.cal_date).collect();
    validate_date_batch(&dates)?;
    let items = dedup_by_date(&req.items, |i| i.cal_date);

    let mut rows = Vec::with_capacity(items.len());
    for item in &items {
        // ⚠️ 覆盖行**不**校验 `CUSTOM_LEAVE` 必须带 name：`t_hr_shift_calendar` 表
        // **没有** `name` 列（那个名字属于全局日历的语义，见迁移注释）。
        // 要写说明走 `remark`。日型合法性照常校验（未知 → 21905）。
        if let Some(remark) = &item.remark {
            check_len("remark", remark, COL_REMARK)?;
        }
        rows.push(repo::NewShiftDay {
            id: snowflake.next_id(),
            cal_date: item.cal_date,
            day_type: DayType::parse(&item.day_type)?.as_str().to_string(),
            remark: item.remark.clone(),
        });
    }

    let affected = repo::upsert_shift_days(conn, id, &rows, current.id, now_naive()).await?;
    if affected == 0 {
        // ⚠️ 写 SQL 里带 `is_active` 守卫（见 `repo::upsert_shift_days`），所以
        // 「一批非空却 0 行」只可能是班次在这两条语句之间被并发停用 / 软删了。
        // 上面那次 `ensure_writable` 是**读-再-写**的 TOCTOU 窗口，这里才是真判据。
        return Err(attribute_zero_row_shift(conn, id).await);
    }
    Ok(ShiftDaysWriteOut { affected })
}

// ============================================================================
// 端点 10：清班次覆盖
// ============================================================================

/// `POST /shifts/{id}/calendar/days/delete` —— 清覆盖，那几天回退到全局日历。
pub async fn delete_shift_days(
    conn: &mut PgConnection,
    current: &CurrentUser,
    id: i64,
    req: &ShiftDaysDeleteRequest,
) -> Result<ShiftDaysWriteOut, AppError> {
    current.require_permission(PERM_MANAGE)?;
    let row = repo::get_shift(conn, id)
        .await?
        .ok_or_else(shift_not_found)?;
    ensure_writable(&row)?;

    let target = resolve_delete_target(req.dates.clone(), req.from, req.to)?;
    let affected = match target {
        DeleteTarget::Dates(ds) => {
            repo::soft_delete_shift_days_by_dates(conn, id, &ds, current.id, now_naive()).await?
        }
        DeleteTarget::Range(f, t) => {
            repo::soft_delete_shift_days_by_range(conn, id, f, t, current.id, now_naive()).await?
        }
    };
    if affected == 0 {
        // 清覆盖的 `affected = 0` 本身是合法成功，但**前提是班次仍可写**：两条软删 SQL
        // 都带班次侧的 `is_active` 守卫（并发停用时它们会命中 0 行）。0 行时回查一次，
        // 把「并发停用」与「那几天本来就没有覆盖行」分开。
        ensure_shift_still_writable(conn, id).await?;
    }
    Ok(ShiftDaysWriteOut { affected })
}

// ============================================================================
// 表驱动单测：validate_segments 的 V1–V6
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// 段数组糖：`[("22:00","26:00"), …]`，`seq` 按序自动编号。
    fn segs(pairs: &[(&str, &str)]) -> Vec<ShiftSegment> {
        pairs
            .iter()
            .enumerate()
            .map(|(i, (s, e))| ShiftSegment::new(i as u8 + 1, s, e))
            .collect()
    }

    /// **合法 6 例** + 逐例的派生列期望。
    ///
    /// 派生列一起钉，是为了让「`overflow_minutes` 不跨天时恒为 0」与
    /// 「`daily_minutes` 是各段之和、无折算」这两条口径不被后来人无声改动。
    #[test]
    fn valid_segments_accepted_with_expected_derived_values() {
        let cases: &[(&str, Vec<ShiftSegment>, bool, i32, i32)] = &[
            (
                "夜班 22:00→次日 02:00（最小跨天形态）",
                segs(&[("22:00", "26:00")]),
                true,
                120,
                240,
            ),
            (
                "24 小时连做 08:00→次日 08:00（span 恰好 24:00，V6 边界）",
                segs(&[("08:00", "32:00")]),
                true,
                480,
                1440,
            ),
            (
                "夜班 + 次日早班（两段都跨天，span 12:00）",
                segs(&[("22:00", "26:00"), ("28:00", "34:00")]),
                true,
                600,
                600,
            ),
            (
                "周末白班两段（不跨天，段间留 1 小时午休）",
                segs(&[("08:00", "12:00"), ("13:00", "17:00")]),
                false,
                0,
                480,
            ),
            (
                "单段 00:00→24:00（end 恰在边界，**不算**跨天）",
                segs(&[("00:00", "24:00")]),
                false,
                0,
                1440,
            ),
            (
                // ⚠️ V5 的**上界**边界：恰好 6 段必须放行（7 段被拒由下面那张表钉）。
                // 只钉「7 段拒」而不钉「6 段过」，那天有人把上界改成 `< 6` 两边都会全绿。
                "三班倒 6 段（V5 上界恰好 6：01:00 起连排到晚 23:00，段间 1 小时休息）",
                segs(&[
                    ("01:00", "05:00"),
                    ("06:00", "10:00"),
                    ("11:00", "15:00"),
                    ("16:00", "18:00"),
                    ("19:00", "21:00"),
                    ("22:00", "23:00"),
                ]),
                false,
                0,
                240 + 240 + 240 + 120 + 120 + 60,
            ),
        ];

        for (desc, input, want_cross, want_overflow, want_minutes) in cases {
            let got = validate_segments(input)
                .unwrap_or_else(|e| panic!("`{desc}` 应当合法，实际被拒：{e}"));
            assert_eq!(got.cross_day, *want_cross, "`{desc}` 的 cross_day");
            assert_eq!(
                got.overflow_minutes, *want_overflow,
                "`{desc}` 的 overflow_minutes"
            );
            assert_eq!(
                got.daily_minutes, *want_minutes,
                "`{desc}` 的 daily_minutes"
            );
        }
    }

    /// **非法 9 例** + 每例期望的错误码。
    ///
    /// 逐例钉到**具体码**（而不是只钉「被拒」）：`21903`（V1–V5 段形态问题）与
    /// `21908`（V6 span 超一天）是两种语义，前端文案不同，混掉就没法只改一条。
    #[test]
    fn invalid_segments_rejected_with_expected_code() {
        struct Case {
            desc: &'static str,
            input: Vec<ShiftSegment>,
            want: i32,
        }
        let seven = segs(&[
            ("01:00", "02:00"),
            ("03:00", "04:00"),
            ("05:00", "06:00"),
            ("07:00", "08:00"),
            ("09:00", "10:00"),
            ("11:00", "12:00"),
            ("13:00", "14:00"),
        ]);
        let cases = vec![
            Case {
                desc: "零长段（start == end，V2 严格小于）",
                input: segs(&[("08:00", "08:00")]),
                want: code::BIZ_HR_SHIFT_SEGMENT_INVALID,
            },
            Case {
                // ⚠️ 与「零长段」同属 V2 但方向相反：start > end 的段是**倒挂**的，
                // 归一化时 start 不小于 end，派生列 `daily_minutes` 会算成负数。
                desc: "倒挂段（start > end，V2 另一侧）",
                input: segs(&[("17:00", "08:00")]),
                want: code::BIZ_HR_SHIFT_SEGMENT_INVALID,
            },
            Case {
                desc: "乱序（第二段 start 早于第一段，V4a）",
                input: segs(&[("13:00", "17:00"), ("08:00", "12:00")]),
                want: code::BIZ_HR_SHIFT_SEGMENT_INVALID,
            },
            Case {
                desc: "段内重叠（第二段 start 早于第一段 end，V4b）",
                input: segs(&[("08:00", "13:00"), ("12:00", "17:00")]),
                want: code::BIZ_HR_SHIFT_SEGMENT_INVALID,
            },
            Case {
                desc: "首段起点 ≥ 24:00（日期归属无从谈起，V3）",
                input: segs(&[("24:00", "28:00")]),
                want: code::BIZ_HR_SHIFT_SEGMENT_INVALID,
            },
            Case {
                desc: "超 48 小时（`49:00`，V1 上界）",
                input: segs(&[("08:00", "49:00")]),
                want: code::BIZ_HR_SHIFT_SEGMENT_INVALID,
            },
            Case {
                desc: "空数组（V5 下界）",
                input: segs(&[]),
                want: code::BIZ_HR_SHIFT_SEGMENT_INVALID,
            },
            Case {
                desc: "超 6 段（V5 上界）",
                input: seven,
                want: code::BIZ_HR_SHIFT_SEGMENT_INVALID,
            },
            Case {
                desc: "span > 24:00（V1–V5 全过，只卡 V6 ⇒ 必须落 21908）",
                input: segs(&[("08:00", "20:00"), ("26:00", "33:00")]),
                want: code::BIZ_HR_SHIFT_SPAN_EXCEEDS_DAY,
            },
        ];

        for c in cases {
            let err = validate_segments(&c.input)
                .err()
                .unwrap_or_else(|| panic!("`{}` 应当被拒，实际被接受", c.desc));
            assert_eq!(err.code(), c.want, "`{}` 的错误码不对：{err}", c.desc);
        }
    }

    /// V6 那一条**必须**落 21908 而不是并进 21903 —— 前端要能只对「跨度超一天」
    /// 给一条专门文案（它同时是「这个班次排法有问题」而不是「参数格式错了」）。
    #[test]
    fn span_over_day_is_a_dedicated_code_and_maps_to_400() {
        let err = validate_segments(&segs(&[("08:00", "20:00"), ("26:00", "33:00")])).unwrap_err();
        assert_eq!(err.code(), code::BIZ_HR_SHIFT_SPAN_EXCEEDS_DAY);
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    /// V6 的**边界**：span 恰好 `24:00` 放行、`24:01` 拒。
    #[test]
    fn span_boundary_is_inclusive_at_exactly_24h() {
        assert!(validate_segments(&segs(&[("08:00", "20:00"), ("20:00", "32:00")])).is_ok());
        let err = validate_segments(&segs(&[("08:00", "20:01"), ("20:01", "32:01")])).unwrap_err();
        assert_eq!(err.code(), code::BIZ_HR_SHIFT_SPAN_EXCEEDS_DAY);
    }

    /// 规范化：`seq` 重编为 `1..n`、段值补零，且**不改变语义**。
    #[test]
    fn normalization_resequences_and_pads_clock_values() {
        let input = vec![
            ShiftSegment::new(9, "8:00", "12:00"),
            ShiftSegment::new(3, "13:00", "17:00"),
        ];
        let got = validate_segments(&input).unwrap();
        assert_eq!(
            got.segments,
            vec![
                ShiftSegment::new(1, "08:00", "12:00"),
                ShiftSegment::new(2, "13:00", "17:00"),
            ]
        );
    }

    /// 段值解析：合法形态全通，越界 / 畸形全拒。
    #[test]
    fn clock_parser_accepts_reasonable_and_rejects_out_of_range() {
        // ⚠️ `24:30`（次日 00:30）是**合法**段值：绝对时钟轴的合法域是
        // `[00:00, 48:00]`，V1 只卡上界 `48:00`。跨日上限由 **V6 的 span** 负责，
        // **不是**由「单个段值不得越过午夜」负责 —— 后者表达不了「夜班 + 次日早班」。
        for ok in ["00:00", "8:00", "08:00", "23:59", "24:00", "24:30", "48:00"] {
            assert!(parse_clock(ok).is_ok(), "`{ok}` 应当合法");
        }
        for bad in [
            "", "0800", "08:0", " 08:00", "08:60", "48:01", "49:00", "100:00", "-1:00",
        ] {
            assert_eq!(
                parse_clock(bad).unwrap_err().code(),
                code::BIZ_HR_SHIFT_SEGMENT_INVALID,
                "`{bad}` 应当被拒"
            );
        }
    }

    /// 区间校验：倒序与超跨度都落 21904（HTTP 400）；`from == to` 与 366 天放行。
    #[test]
    fn range_validation_boundaries() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        assert!(validate_range(d, d).is_ok());
        assert!(validate_range(d, d + chrono::Duration::days(366)).is_ok());
        assert!(validate_range(d, d + chrono::Duration::days(367)).is_err());
        let err = validate_range(d + chrono::Duration::days(1), d).unwrap_err();
        assert_eq!(err.code(), code::BIZ_HR_CALENDAR_RANGE_INVALID);
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    /// 删行目标的两种形态归一：二选一命中才过，四种非法形态全落 21904。
    #[test]
    fn delete_target_requires_exactly_one_form() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        assert!(matches!(
            resolve_delete_target(Some(vec![d]), None, None).unwrap(),
            DeleteTarget::Dates(_)
        ));
        assert!(matches!(
            resolve_delete_target(None, Some(d), Some(d + chrono::Duration::days(7))).unwrap(),
            DeleteTarget::Range(_, _)
        ));
        for bad in [
            resolve_delete_target(None, None, None),
            resolve_delete_target(None, Some(d), None),
            resolve_delete_target(None, None, Some(d)),
            resolve_delete_target(Some(vec![d]), Some(d), Some(d)),
            resolve_delete_target(Some(vec![]), None, None),
        ] {
            assert_eq!(bad.unwrap_err().code(), code::BIZ_HR_CALENDAR_RANGE_INVALID);
        }
    }

    /// **批量条数上限**：一条 `items` 攒 367 项（哪怕全是同一个日期）必须被拒成
    /// `21904`。
    ///
    /// ⚠️ 这条与「跨度 ≤ 366 天」是**两个**判据：367 个**同日期**的项跨度恒为 0，
    /// 只卡跨度会让它一路灌进一次巨大的 `UNNEST`。
    #[test]
    fn batch_item_count_is_capped_even_when_every_date_is_identical() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        assert!(
            validate_date_batch(&vec![d; MAX_BATCH_ITEMS]).is_ok(),
            "恰好 {MAX_BATCH_ITEMS} 条放行"
        );
        let err = validate_date_batch(&vec![d; MAX_BATCH_ITEMS + 1]).unwrap_err();
        assert_eq!(err.code(), code::BIZ_HR_CALENDAR_RANGE_INVALID);
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    /// **同批重复日期按「后写覆盖先写」收敛**（PG 的 `21000` 那一刀的前置防线）。
    ///
    /// ⚠️ 顺序也要钉：结果保留**第一次出现的位置**，只把值换成后写那条。这样
    /// `affected` 与入参顺序的关系不会因为去重而变得不可预期。
    #[test]
    fn dedup_by_date_keeps_last_write_in_the_first_occurrence_slot() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let d2 = d + chrono::Duration::days(1);
        let items = vec![("a", d), ("b", d2), ("c", d), ("d", d2), ("e", d2)];
        let kept = dedup_by_date(&items, |(_, d)| *d);
        let tags: Vec<&str> = kept.iter().map(|(t, _)| *t).collect();
        assert_eq!(tags, vec!["c", "e"], "后写覆盖先写，且保留首次出现的位置");
        assert_eq!(kept[0].1, d);
        assert_eq!(kept[1].1, d2);
    }

    /// 列宽守卫：**按字符数**判，且四列全部越界落 `21909` / HTTP 400（不是 500）。
    ///
    /// ⚠️ 中文那条是关键：`varchar(30)` 放得下 30 个汉字（90 字节）。按字节判会把
    /// 「10 个汉字的 code」误拒 —— 挡住合法输入比 500 更坏。
    #[test]
    fn check_len_counts_characters_and_maps_to_21909() {
        let han = "汉"; // 1 字符 = 3 字节
        assert!(
            check_len("code", &han.repeat(30), 30).is_ok(),
            "30 个汉字放行"
        );
        let err = check_len("code", &han.repeat(31), 30).unwrap_err();
        assert_eq!(err.code(), code::BIZ_HR_FIELD_INVALID);
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);

        for (field, max) in [
            ("code", COL_SHIFT_CODE),
            ("name", COL_NAME),
            ("color", COL_COLOR),
            ("remark", COL_REMARK),
        ] {
            assert!(
                check_len(field, &"x".repeat(max), max).is_ok(),
                "{field} 边界放行"
            );
            assert_eq!(
                check_len(field, &"x".repeat(max + 1), max)
                    .unwrap_err()
                    .code(),
                code::BIZ_HR_FIELD_INVALID,
                "{field} 超宽应落 21909"
            );
        }
    }

    /// 必填文本：空白 / 空串一律 `21909`，且落库前已 `trim()`。
    ///
    /// ⚠️ 空白 `code` 会**占住** `uk_t_hr_shift_code` 的 partial unique —— 建出一条
    /// `code = ''` 的班次后，所有想用空 code 的人都会撞 `21902`，而那条空班次本身没有
    /// 任何业务含义。
    #[test]
    fn require_text_rejects_blank_and_trims() {
        assert_eq!(require_text("code", "  DAY  ").unwrap(), "DAY");
        for blank in ["", " ", "\t", "   \t  "] {
            let err = require_text("code", blank).unwrap_err();
            assert_eq!(err.code(), code::BIZ_HR_FIELD_INVALID, "`{blank}` 应被拒");
            assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        }
    }

    /// 读侧降级：库里那一列 jsonb 被改坏时落 `21903`，**不是** 500。
    #[test]
    fn corrupt_segments_json_degrades_to_21903_not_500() {
        for bad in [
            serde_json::json!("not-an-array"),
            serde_json::json!([1, 2, 3]),
            serde_json::json!([{"seq": 1, "start": "99:99", "end": "12:00"}]),
            serde_json::json!([]),
        ] {
            let err = parse_segments_strict(&bad).unwrap_err();
            assert_eq!(
                err.code(),
                code::BIZ_HR_SHIFT_SEGMENT_INVALID,
                "坏 jsonb {bad} 应当降级成 21903"
            );
        }
    }

    /// `day(offset)` 的 ISO 串（推导用例的确定性基准日 = 2030-03-04 周一）。
    fn d(offset: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2030, 3, 4).unwrap() + chrono::Duration::days(offset)
    }

    /// 端点 1 的 `shift_bands` 形状（2026-10-19 新增）：**无覆盖行 ⇒ `effective` 逐字
    /// 等于全局日型、`is_override = false`**；**有覆盖行 ⇒ 等于覆盖值、`is_override =
    /// true`**。
    ///
    /// ⚠️ 同时钉住「覆盖 map 的键**含 `shift_id`**」：B 班次的覆盖行不得渗进 A 班次的
    /// 色带 —— 那正是 `HashMap<(i64, NaiveDate), _>` 这个复合键存在的全部理由，
    /// 换成按日期单键时这条会立刻全绿地挂掉。
    #[test]
    fn shift_band_shape_follows_the_override_row_existence() {
        let mon = d(0);
        let globals: HashMap<NaiveDate, String> = [(mon, "STATUTORY_HOLIDAY".to_string())]
            .into_iter()
            .collect();
        let overrides: HashMap<(i64, NaiveDate), String> =
            [((11_i64, mon), "MAKEUP_WORKDAY".to_string())]
                .into_iter()
                .collect();

        // 有覆盖行（班次 11）
        let (g, eff, is_ovr) = derive_effective(&globals, &overrides, 11, mon).unwrap();
        assert_eq!(g, DayType::StatutoryHoliday, "global 仍是全局那一档");
        assert_eq!(eff, DayType::MakeupWorkday, "覆盖行压过全局行");
        assert!(is_ovr, "有覆盖行 ⇒ is_override");

        // 无覆盖行（班次 22，同一个 map）⇒ 逐字继承全局
        let (g, eff, is_ovr) = derive_effective(&globals, &overrides, 22, mon).unwrap();
        assert_eq!(g, DayType::StatutoryHoliday);
        assert_eq!(eff, g, "无覆盖行 ⇒ effective 逐字等于 global");
        assert!(!is_ovr, "无覆盖行 ⇒ is_override=false");

        // 全局也没有显式行 ⇒ 周一兜底 WORKDAY（`REST_DAY` 只落在周末）
        let sat = d(5);
        let empty: HashMap<NaiveDate, String> = HashMap::new();
        let (_, eff, is_ovr) = derive_effective(&empty, &overrides, 22, sat).unwrap();
        assert_eq!(eff, DayType::RestDay, "周六兜底 REST_DAY");
        assert!(!eff.is_workday());
        assert!(!is_ovr);
    }

    /// 端点 1 的 `shift_bands` 里，库里出现未知日型字面量时降级成 `21905`（不是 500）。
    ///
    /// ⚠️ **全局行与覆盖行两侧都要钉**：只钉一侧时，另一侧改成「原样透传字符串」这种
    /// 退法测试不会红，而前端会拿到一个不认识的日型字面量。
    #[test]
    fn unknown_day_type_in_both_maps_degrades_to_21905() {
        let mon = d(0);
        for globals in [
            [(mon, "WORKDAY".to_string())]
                .into_iter()
                .collect::<HashMap<_, _>>(),
            [(mon, "不是日型".to_string())]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        ] {
            let overrides: HashMap<(i64, NaiveDate), String> =
                [((11_i64, mon), "也不是日型".to_string())]
                    .into_iter()
                    .collect();
            let err = derive_effective(&globals, &overrides, 11, mon).unwrap_err();
            assert_eq!(err.code(), code::BIZ_HR_DAY_TYPE_INVALID);
            assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        }
    }

    /// ⚠️ **回归**：`ShiftBandOut` 的 wire 形状 —— 恰好 **4 个键**、`shift_id` 是
    /// **JSON string**（雪花防 JS 精度截断）、`is_workday` 恒等于
    /// `effective_day_type.is_workday()`。
    ///
    /// 钉键的个数而不是「某个键存在」：多塞一个字段（如 `code` / `color`）会让
    /// 前端多一份**与 `GET /shifts` 不同步**的班次真相源，那是最坏的一种冗余。
    #[test]
    fn shift_band_serializes_with_exactly_four_keys_and_string_shift_id() {
        let v = serde_json::to_value(ShiftBandOut {
            shift_id: 7_376_123_456_789_012_345,
            effective_day_type: DayType::MakeupWorkday,
            is_workday: true,
            is_override: true,
        })
        .unwrap();
        let obj = v.as_object().expect("是对象");
        assert_eq!(
            obj.len(),
            4,
            "恰好 4 个键，多一个都是「第二个班次真相源」：{v}"
        );
        assert_eq!(
            obj["shift_id"],
            serde_json::json!("7376123456789012345"),
            "雪花 id 出参必须是 JSON string：{v}"
        );
        assert_eq!(
            obj["effective_day_type"],
            serde_json::json!("MAKEUP_WORKDAY")
        );
        assert_eq!(obj["is_workday"], serde_json::json!(true));
        assert_eq!(obj["is_override"], serde_json::json!(true));

        // `is_workday` 与日型的对应关系由 `DayType::is_workday` 单点决定，两档之外
        // 一律 false（前端色带按这个值上色）。
        for (dt, want) in [
            (DayType::Workday, true),
            (DayType::MakeupWorkday, true),
            (DayType::RestDay, false),
            (DayType::StatutoryHoliday, false),
            (DayType::MakeupHoliday, false),
            (DayType::CustomLeave, false),
        ] {
            let band = serde_json::to_value(ShiftBandOut {
                shift_id: 1,
                effective_day_type: dt,
                is_workday: dt.is_workday(),
                is_override: false,
            })
            .unwrap();
            assert_eq!(
                band["is_workday"],
                serde_json::json!(want),
                "{:?} 的 is_workday 应为 {want}",
                dt
            );
        }
    }
}
