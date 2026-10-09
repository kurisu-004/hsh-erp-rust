//! statistics 域 worker 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

use super::overview::DayCount;

/// 有效工时（分钟）的**单一**定义，所有字段的 doc 都指向
/// [`crate::modules::statistics::repo::sql`] 的 `effective_minutes` 模块，
/// 口径细则（事件集合 / LATERAL 去重 / 锚链来源 / 链外工序计 0）在那里逐条
/// 写死 —— 这里只复述结论，避免两处各写一份必然漂移。
///
/// - **单位是分钟整数**（`i64`），**不是小时、不是浮点**。`f64` 工时在前端
///   累加会出现 `0.1 + 0.2` 类误差，且本仓所有时长类字段都是整数。
/// - **计入**：`RETURNED_TO_SHELF` / `SENT_TO_INSPECTION` 两种「工人完成了一
///   道工序」的事件。
/// - **不计入**：`PICKED_UP` / `TAKEN_FROM_POOL` / `MOVED`（那是**开始**，一进
///   一出都计会把同一道工序计两次）、`INSPECTED`（品检员动作）、`DELIVERED`
///   （无工序语义）。
/// - ⚠️ **链外工序计 0 分钟，不是丢弃该行**：`process_id IS NULL` /
///   `t_part.process_chain_id IS NULL` / 链内找不到该 `process_id` 对应的
///   step，这三种情况一律 `effective_minutes` 贡献 0，但同一行照常计入
///   `worked_process_count` 与 `worked_batch_count`。
/// - ⚠️ **取工时用 `LEFT JOIN LATERAL … ORDER BY sort_order, id LIMIT 1`**：
///   `t_process_chain_step` **没有** `(chain_id, process_id)` 唯一约束，同名
///   工序在一条链里出现多次是后端照收的合法形态；直接 JOIN 会让工时按命中条数
///   **翻倍**且**不报错**。
/// - ⚠️ **数据覆盖受工序链配置率限制**：`effective_minutes` 只在**已配工序链**
///   的工单上有值，未配链的工单恒为 0。看到大面积 0 先查链配置率，不是回归。

#[derive(Debug, Clone, Serialize)]
pub struct WorkerStatsItem {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    pub worker_name: String,
    pub badge_code: String,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub work_type_id: Option<i64>,
    pub work_type_name: Option<String>,
    pub is_active: bool,

    pub pickup_count: i64,
    pub pickup_quantity: i64,
    pub participated_part_count: i64,
    /// 贡献度百分比 0-100；公式见 StatisticsService::_compute_contribution
    pub contribution_pct: Option<f64>,

    /// 有效工时（**分钟**整数）。计入 / 不计入的事件集合、链外工序计 0、
    /// LATERAL 去重、数据覆盖受链配置率限制 —— 见本文件顶部注释。
    pub effective_minutes: i64,
    /// 期内完成过的**不同工序**数（`COUNT(DISTINCT e.process_id)`）。
    /// ⚠️ 链外工序**照常计入**（不是丢弃该行），但 `process_id IS NULL` 的行
    /// 不贡献 distinct 值 —— SQL 的 `COUNT(DISTINCT)` 天然忽略 NULL，与
    /// `effective_minutes` 的「计 0」是两件事。
    pub worked_process_count: i64,
    /// 期内作业过的**不同批次**数（`COUNT(DISTINCT e.batch_id)`）。同样天然
    /// 忽略 `batch_id IS NULL`。
    pub worked_batch_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerStatsListOut {
    pub items: Vec<WorkerStatsItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerBrief {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub badge_code: String,
    pub work_type_name: Option<String>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerPartItem {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub status: String,
    pub pickup_count: i64,
    pub last_pickup_at: NaiveDateTime,
}

/// 一行 = 一道工序在该工人区间内的作业汇总（`GET /workers/{id}` 的
/// `process_breakdown[]` 元素）。
///
/// `process_id` **可空**不是偷懒：写侧只有 `prod::scan` 的 **4** 个事件构造点落
/// `process_id`（`worker_scan` 的 `RETURNED_TO_SHELF` / `SENT_TO_INSPECTION` 与
/// `pickup` 的 `SPLIT` / `PICKED_UP`，均取 `batch.current_process_id`），其余
/// `t_part_event` 写点一律 `NULL`，这类事件在按工序分组时汇成一个「无工序」组 ——
/// 它对 `effective_minutes` 总额的贡献恒为 0。逐点取值依据见
/// `crate::shared::part_event` 的模块 doc。雪花 ID 序列化成 JSON string
/// （`serialize_i64_opt`），与 [`WorkerPartItem::part_id`] 同款。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerProcessBreakdown {
    #[serde(serialize_with = "serialize_i64_opt")]
    pub process_id: Option<i64>,
    /// 工序显示名。取不到时退 `(已删除)` 字符串，与
    /// `com::delivery_note::service::group` 的既有兜底同款。
    ///
    /// ⚠️ **`t_process` 行已软删**也是取得到名的那一支：breakdown 的
    /// `LEFT JOIN t_process pr` 刻意**不**在 ON 上带 `pr.deleted_at IS NULL`
    /// （理由见 `sql.rs` 的 `PROCESS_JOIN_SQL`）—— 事后软删是对当前工序字典的
    /// 行政操作，不追溯抹掉历史报表里的名字。⇒ 真正落到本兜底的只剩
    /// `process_id IS NULL` 与工序行物理缺失两种。
    /// ⚠️ `process_id` 为 `null` 与本字段为 `(已删除)` 是**两种不同的「无名」**，
    /// 前端别混用同一个占位文案。
    pub process_name: String,
    /// 该工序在该区间内被「完成」的次数（`COUNT(*)`，不是 distinct）。
    pub step_count: i64,
    /// 该工序贡献的有效工时（**分钟**整数）；口径见本文件顶部注释。
    pub effective_minutes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerDetailOut {
    pub worker: WorkerBrief,
    pub pickup_count: i64,
    pub pickup_quantity: i64,
    pub participated_part_count: i64,
    pub return_count: i64,
    pub daily_pickups: Vec<DayCount>,
    pub parts: Vec<WorkerPartItem>,
    /// 有效工时总额（**分钟**整数），等于 [`Self::process_breakdown`] 各行
    /// `effective_minutes` 之和（同源 SQL、同一组事件，见 `sql.rs` 的
    /// `effective_minutes` 模块）。链外工序计 0、数据覆盖受链配置率限制。
    pub effective_minutes: i64,
    /// 按工序拆分（已按 `effective_minutes DESC, step_count DESC, process_id
    /// ASC` 排序，服务端排好，前端不要再排）。
    pub process_breakdown: Vec<WorkerProcessBreakdown>,
}
