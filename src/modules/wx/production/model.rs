//! wx::production 子模块 model 层 —— `FromRow` 行结构
//!
//! 2026-10-11 新增。自旧 `src/modules/wx/repo.rs` 的 `WxBatchRow` 搬入并按本域
//! VO 契约裁剪（`part_id` / `drawing_url` 两个字段不再进 VO，故也不再进 model 的
//! 必填位）。
//!
//! ## ⚠️ 刻意**不** derive `Serialize`
//! `model.rs` 是「SQL 投影的原始快照」，不是响应结构。JSON 契约的唯一真源是
//! [`super::vo`]：由 service 层显式投影（字段名 camelCase、雪花 id 字符串化、
//! 无 `drawingUrl`…）。给 model 加 `Serialize` 等于开一条「绕过 service 直接把
//! 行结构当响应」的旁路，那条旁路上没有任何 serde 属性，字段名全是 snake_case ——
//! 与 `docs/api/wx.md` §2 的契约逐字不符。同 `wx::part_list::model` 的取舍。

use chrono::NaiveDate;
use sqlx::FromRow;

/// `t_part_batch` 的一行（+ 工单 4 列 + 持有人名 + 两个事件派生列）。
///
/// 字段与 [`super::repo`] 的 `SELECT_COLS` 列**一一对应**（alias 名逐字对齐字段名），
/// `FromRow` 才能取到值。
#[derive(Debug, Clone, FromRow)]
pub struct ProductionBatchRow {
    /// `t_part_batch.id`（雪花 ID → VO 里序列化成 JSON string）
    pub id: i64,
    /// `t_part.serial_no`（可为 NULL —— 手工工单没序列号）
    pub serial_no: Option<String>,
    /// `t_part.name`
    pub name: String,
    /// `t_part.drawing_no`（前端卡片里的 `code`，即「图号」）
    pub drawing_no: String,
    /// `t_part_batch.batch_no`（**数字**；前端自己 `padStart(2, '0')` 补零）
    pub batch_no: i32,
    /// `t_part_batch.quantity`（**本批次**件数，见 `vo.rs` 的口径陷阱登记）
    pub quantity: i32,
    /// `t_part_batch.status`（DB 原值；VO 里被折叠成 `in_progress` / `done`）
    pub status: String,
    /// 持有人名：`t_worker.name`，仅当 `location = 'WORKER'` 且 holder 命中
    /// `t_worker` 时非空。批次挂在货架上时为 NULL。
    pub assigned_to: Option<String>,
    /// `t_part.planned_delivery_date`（NOT NULL，恒有值）
    pub planned_delivery_date: NaiveDate,
    /// 最近一次 `DELIVERED` 事件的日期（子查询，无该事件时 NULL）
    pub finished_date: Option<NaiveDate>,
    /// 该批次 `PICKED_UP + RETURNED` 事件的 `SUM(quantity)`。
    ///
    /// ⚠️ **刻意不写 `COALESCE(..., 0)`**：旧 SQL 写了，于是「本月零事件」的批次
    /// 拿到 `0` 而不是 `null`。前端 `<part-card>` 用 `wx:if="{{item.workHours !=
    /// null}}"` 守门 —— 给 `0` 会让「没工时」被渲染成「0 小时工时」。见
    /// `vo.rs` 的「`workHours` 的 null 语义」段与 `docs/api/wx.md` §8.6。
    pub work_hours: Option<f64>,
}

/// 登录账号 → 工人（`t_user.worker_id → t_worker` → `t_work_type`）的一行。
///
/// ⚠️ 这条链路是 2026-10-11 B3 修掉的核心 bug：旧 `/wx/worker/stats` 直接把
/// `CurrentUser.id`（= `t_user.id`）当 `t_part_event.worker_id`（= `t_worker.id`）
/// 用，而两表之间**没有任何映射**（实测 5750 条事件的 13 个 worker_id 全部只命中
/// `t_worker`、零命中 `t_user`）⇒ 该端点对任何真实用户恒返 `batch_count: 0`。
/// B1 已加 `t_user.worker_id bigint` 列（无物理 FK），本域按该列解链。
#[derive(Debug, Clone, FromRow)]
pub struct WorkerRow {
    /// `t_worker.id`（= `t_user.worker_id`）。**进 stats 查询的过滤锚点**。
    pub worker_id: i64,
    /// `t_worker.name`（NOT NULL）
    pub name: String,
    /// `t_work_type.name`（`LEFT JOIN` + `deleted_at IS NULL`）：
    /// - 工人未分配工种（`t_worker.work_type_id IS NULL`）→ NULL
    /// - 工种行已软删 → NULL
    ///
    /// service 层把 NULL 归一成**空串**（见 `vo.rs` 的 `WorkerOut::work_type`）。
    pub work_type_name: Option<String>,
}

/// 工人当月工作量聚合行（单条 `t_part_event` 聚合查询的产物）。
#[derive(Debug, Clone, FromRow)]
pub struct WorkerStatsRow {
    /// 该工人当月发生过事件的**不同 `batch_id`** 数（`batch_id IS NULL` 的事件不计）
    pub batch_count: i64,
    /// 该工人当月 `PICKED_UP + RETURNED` 事件的 `SUM(quantity)`（无事件为 `0`）
    ///
    /// ⚠️ **工作量估算**，不是真实工时：DB schema 无 `work_hours` 列，用「加工
    /// 件数」顶替（沿自旧实现，也是 `statistics` 域的同形口径）。
    pub qty_sum: i64,
}

/// 批次 tab 角标（由**两条**标量查询在 repo 层拼装，故不是 `FromRow` 行结构）。
#[derive(Debug, Clone)]
pub struct BatchCountsRow {
    /// 当月 `IN_PROCESS` 且 `updated_at` 落在当月的批次数
    pub in_progress: i64,
    /// 当月存在 `DELIVERED` 事件、且批次 `status IN ('DELIVERED','COMPLETED')` 的批次数
    pub done: i64,
}
