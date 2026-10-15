//! `hr` 域 —— 人事排班（全局日历 + 班次 + 班次日历覆盖），**2026-10-19 新建**
//!
//! 业务目标：给考勤排班打基础。本期是**纯配置域**，**不做班次与实际工人的绑定**
//! （那属于排班实例与考勤流水，不在本期范围）。
//!
//! ## 端点（10 条，挂在 `/api/v2/hr`，由 [`router`] 桥接）
//!
//! | # | Method | Path | 权限点 | 说明 |
//! |---|---|---|---|---|
//! | 1 | `GET` | `/calendar/days?from&to` | `hr.view` | 全局日历区间读（逐日一行，`derived` 标出推导日） |
//! | 2 | `POST` | `/calendar/days` | `hr.schedule.manage` | 批量 upsert 全局日历显式行（**只存例外**） |
//! | 3 | `POST` | `/calendar/days/delete` | `hr.schedule.manage` | 删显式行 → 回退按星期推导 |
//! | 4 | `GET` | `/shifts` | `hr.view` | 班次列表（含三个派生列） |
//! | 5 | `POST` | `/shifts` | `hr.schedule.manage` | 建班次 |
//! | 6 | `POST` | `/shifts/{id}` | `hr.schedule.manage` | 改班次（部分覆盖 + `version` OCC 锚） |
//! | 7 | `POST` | `/shifts/{id}/deactivate` | `hr.schedule.manage` | 停用（软下架，**不级联**覆盖行） |
//! | 8 | `GET` | `/shifts/{id}/calendar?from&to` | `hr.view` | 班次区间日历（`global` 与 `effective` 都回传） |
//! | 9 | `POST` | `/shifts/{id}/calendar/days` | `hr.schedule.manage` | 批量 upsert 班次覆盖行 |
//! | 10 | `POST` | `/shifts/{id}/calendar/days/delete` | `hr.schedule.manage` | 清覆盖 → 回退全局日历 |
//!
//! **权威路由清单是 `handler::ROUTES`（10 条）**，lib 单测
//! `handler::tests::routes_declared_in_router` 用
//! `crate::modules::files::route_guard::routes_in_router_source` 逐条比对 `router()`
//! 源码（复用该共用比对器，**不**复制一份）。
//!
//! ## 日历模型：全局日历 + 班次单日覆盖，**没有独立的日历实体**
//!
//! 班次的「自己的工作日历」= 全局日历 ∪ 本班次覆盖行。**不是独立实体，没有
//! `calendar_id` 列** —— 三张表：`t_hr_calendar_day`（全局）、`t_hr_shift`（班次）、
//! `t_hr_shift_calendar`（班次覆盖）。
//!
//! 推导规则（**唯一真源在后端，前端不重复实现**）：
//!
//! ```
//! effective_day_type(shift, date) =
//!     班次覆盖行(day_type)                          若存在
//!   | 全局日历行(day_type)                          若存在
//!   | 按星期推导（周六周日 = REST_DAY，否则 WORKDAY）  兜底
//!
//! is_workday = day_type ∈ { WORKDAY, MAKEUP_WORKDAY }
//! ```
//!
//! ⚠️ **日历表只存例外**：常态工作日 / 常态周末**不落行**，读端点无显式行时按星期
//! 推导并把该行标 `derived = true`。365 天全落行会让「明年元旦调休」从「改一行」
//! 变成「改 200 行」。
//!
//! ## 班次时段：**绝对时钟轴**（本设计最关键的一刀）
//!
//! 段值域是 `cal_date 00:00` 起算的**绝对偏移**，字符串 `"HH:MM"`，`HH ∈ 0..48`。
//! 夜班 22:00 → 次日 02:00 写 `start = "22:00"` / `end = "26:00"`。
//!
//! **「跨天班次的日期归属 = 工作开始时间所在日」由数据结构本身保证**，不需要任何
//! 推断逻辑，也不需要「跨天标志位」。
//!
//! **校验 V1–V6**（全在 `service::validate_segments`，表驱动单测）：
//!
//! | # | 规则 | 违规码 |
//! |---|---|---|
//! | V1 | `0 ≤ start`，`end ≤ "48:00"` | 21903 |
//! | V2 | `start < end`（严格，**不承认零长段**） | 21903 |
//! | V3 | `segments[0].start < "24:00"`（首段必须落在当日） | 21903 |
//! | V4 | 按 `start` 升序且不重叠：`start_{i+1} >= end_i` | 21903 |
//! | V5 | `1 ≤ n ≤ 6` | 21903 |
//! | **V6** | **`span = max(end) − min(start) ≤ "24:00"`** | **21908** |
//!
//! ### ⚠️ V6 为什么是本域最关键的一条
//!
//! **跨日重叠 ⟺ `span > 24h`**。卡死 V6 之后，相邻两日 / 隔日的班次实例在时间轴上
//! **结构性地不可能重叠** ⇒ **日历写端点零重叠检查**（它们只需要「同一班次同一天
//! 只有一条覆盖行」这条同日语义，那由 `uk_t_hr_shift_calendar` 守着）。
//! span ≤ 24h 同时也是「一个班次实例最长 24 小时」的劳动法底线。
//!
//! 放宽 V6 的代价不是「多一种排班形态」，而是日历写端点必须新增一条跨
//! `days × shifts` 的区间求交检查、且与并发插入之间存在 TOCTOU 窗口。
//!
//! ## 错误码段：**219xx**（2026-10-19 新增）
//!
//! ⚠️ **不是 216xx**：该段（上传会话域）已于 2026-09-28 随域下线**释放**，但复用
//! 一个「已用过又被回收」的段位有风险 —— 老客户端 / 旧日志里可能还残留 216xx 的映射，
//! **撞码比撞段更隐蔽**（217xx 当初就是为此放弃了 216xx）。`218xx` 已被 wecom 占用
//! （21801–21808）⇒ 取全新段 219xx。逐码语义见 `shared::error::code` 的 219xx 段声明。
//!
//! | 码 | 常量 | HTTP | 触发场景 |
//! |---|---|---|---|
//! | 21901 | `BIZ_HR_SHIFT_NOT_FOUND` | 404 | 查 / 改 / 停用一个不存在或已软删的班次 |
//! | 21902 | `BIZ_HR_SHIFT_CODE_DUPLICATED` | 409 | 班次 `code` 重复（**不是** 40901 OCC） |
//! | 21903 | `BIZ_HR_SHIFT_SEGMENT_INVALID` | 400 | 段非法 / 库里那一列 jsonb 解不出来 |
//! | 21904 | `BIZ_HR_CALENDAR_RANGE_INVALID` | 400 | `from > to` / 跨度 > 366 天 / upsert 或 delete 的批量**条数** > 366 / upsert 的 `items` 为空 / 删行请求没恰好命中 `{dates}` 或 `{from,to}` 一种形态 |
//! | 21905 | `BIZ_HR_DAY_TYPE_INVALID` | 400 | 未知 `day_type` |
//! | 21906 | `BIZ_HR_SHIFT_INACTIVE` | 409 | 对已停用班次做写操作 |
//! | 21907 | `BIZ_HR_CALENDAR_LEAVE_NAME_REQUIRED` | 400 | `CUSTOM_LEAVE` 未填 `name` |
//! | 21908 | `BIZ_HR_SHIFT_SPAN_EXCEEDS_DAY` | 400 | **V6**：`span > 24:00` |
//! | 21909 | `BIZ_HR_FIELD_INVALID` | 400 | 必填项（`code` / `name`）trim 后为空白，或某字段超 `varchar` 列宽（判**字符数**） |
//!
//! ## 结构（**转发式**，见 CLAUDE.md「路由声明规约」第 9 条）
//!
//! `mod.rs` 只放 `pub fn router()`，路由表在 `handler::router()`。
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `model.rs` | `DayType`（6 档）+ `ShiftSegment` + 三张表的 `T*` 全列投影 |
//! | `dto.rs` | 入参 10 组 + 字面量常量（权限点、区间上限、段数与时钟上界） |
//! | `vo.rs` | 出参（含 `cross_day` / `overflow_minutes` / `daily_minutes` 派生） |
//! | `repo.rs` | SQL 真源 + `ALIVE` 守卫谓词，**全部运行时 `query_as`** |
//! | `service.rs` | `validate_segments`（V1–V6）+ 10 条用例编排 + 区间校验 |
//! | `handler.rs` | 10 端点 + `ROUTES` + `router()` + 3 个 `#[cfg(test)]` 单测 |
//!
//! ## ⚠️ SQL 路线：**运行时 `query_as`，不进 `query!` 宏**
//!
//! `t_hr_shift.segments` 是本仓**第一个 jsonb 列**，而 `.sqlx/` 的 281 份离线元数据
//! 里 **0 个 `JSONB` type_info** ⇒ `query!` 宏 + jsonb 是**零先例组合**。理由与取舍
//! 全文见 `repo.rs` 模块 doc。**因此本域不需要跑 `./scripts/sqlx_prepare.sh`、
//! 不产生 `.sqlx/` 缓存 diff。**
//!
//! ## 域隔离护栏：**刻意不装**（2026-10-19 复核，2026-10-23 复核措辞）
//!
//! 本域的业务代码**零跨域引用** —— 三张表 `t_hr_calendar_day` / `t_hr_shift` /
//! `t_hr_shift_calendar` 全是本域自有，排班与工人 / 订单 / 考勤零交集，SQL 真源
//! 全部写在 `repo.rs` 里。
//!
//! 但**整域护栏仍装不上**：`shared::domain_guard::assert_no_foreign_domain` 是**按
//! 路径前缀**判定的（扫到任何 `modules::<其它域>` 路径即失败），而 `handler.rs` 的
//! `mod tests` 按仓内统一约定**必须**复用共用比对器
//! `crate::modules::files::route_guard::routes_in_router_source`（复制一份会让
//! 「解析规则」变成两份可漂移的散文 —— 见 `files::route_guard` 模块 doc 的立论）。
//! 那一行是本域**唯一**的跨域路径，且它**既不是 service 也不是 repo**，只是一条
//! `#[cfg(test)]` 的路由表比对器。
//!
//! 护栏的规则（「不 import 别域的 service / repo」）在这里恰好把**正确**的写法也拦了。
//!
//! ⚠️ **与 `serial` / `print_template` 的差别要说清（不要笼统写成「同款」）**：那两域
//! 之所以不装护栏，是因为它们的**生产代码**里就 import 了别域（`assembly::repo` /
//! `files::part_file`），护栏即使放过 `#[cfg(test)]` 也会照样报警；而本域唯一的跨域
//! 路径**只在 `#[cfg(test)]` 里**，生产代码零跨域。三处都「不装护栏」，成因并不同款。
//!
//! 日后若要把 `route_guard` 这类「共用测试工具」上提到 `shared`（那才是它该在的
//! 位置），本节应改成「装护栏」（生产代码侧确实已经干净）。
//! **不要**为了「让本域看起来整齐」而删掉 `handler::tests` 里那行比对器复用。
//!
//! ## 零 WS 广播（有意取舍）
//!
//! 本域是**配置页**，**没有订阅方**：大屏 WS（`/ws/dashboard`）的增量清单里没有排班
//! 数据，仓库当前也没有任何「排班变更」的消费端。加一条没有订阅者的广播等于给每个
//! 大屏客户端白加一次防抖与重取。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo;

/// 本域 router 工厂（转发式：路由表在 [`handler::router`]）。
///
/// 挂载点：`src/modules/mod.rs::v2_router()` 的 `.nest("/hr", …)`
/// —— 顶层 nest、单段裸名词（`hr`，不是 kebab-case）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
