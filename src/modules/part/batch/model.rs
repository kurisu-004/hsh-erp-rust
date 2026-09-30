//! part_batch 域数据模型
//!
//! 对应 Python myERP/model/part_batch.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只投影 delivery_note / delivery_group 后续会用到的列：
//! 标识 + 工单 + 批次号 + 数量 + 状态 + 位置 + holder + current_process_step +
//! 送货单关联 + 父批次 + 乐观锁 + 软删。
//! Python `TPartBatch` 的其他字段留到 part_batch 域实施阶段扩展。
//!
//! 2026-09-16 PR-2 瘦身（migration 027）：删 `has_been_repaired` —— 拆批后
//! 无法确定是哪一个批次返修，列语义失真，整体废弃（返修事实仍可追溯
//! `t_part_event` 的 REPAIR_STARTED 事件）。
//!
//! 2026-09-16 PR-3 批次 step 化（migration 028）：
//! - 删 `next_process_id`（t_part_batch 列），改 `current_process_step_id`
//!   指向所属 part 的工艺链步骤（t_process_chain_step.id）
//! - 删 `placed_at`（不再统计生产时间）
//!
//! 真实 process_id 原先由 service 层 JOIN t_process_chain_step 按需派生
//! （2026-09-30 起不再需要，见下条）。
//!
//! 2026-09-30 新增 `current_process_id`（migration 004）：
//! - `current_process_id`（逻辑 FK → `t_process.id`）是**判断批次是否属于某
//!   工序池的唯一权威依据**：worker_pool 候选池 3 条 SQL + count 全部按本列
//!   普通过滤（不再 JOIN `t_process_chain_step`）
//! - `current_process_step_id` 相应**降级为可选的显示用定位信息**：仅当工单已
//!   绑定工序链时才写，允许 NULL；且**只在首次定位工序时写、之后不再推进**
//!   （2026-09-30 review 第 3 轮订正措辞，详见 `TPartBatch` 字段 doc）
//! - 目的：让**没有工序链的工单，其批次也能正常入池**（旧设计下 dispatch 写
//!   `step=NULL` + 候选池 SQL INNER JOIN step → 批次对所有池查询隐身，形成
//!   「要推进 step 先进池、要进池先有 step」的死状态）
//!
//! **读取方分工**（2026-09-30 review 第 3 轮 M3 确立，勿越界）：
//!
//! - `current_process_id` 的读取方严格限定为 **5 条工序池 SQL**（take_one /
//!   take_specific / list_candidates / group_count / count_pool_by_shelf）
//!   + `list_pickable_by_work_type` + rollup 派生 `t_part.next_process_id`。
//! - **展示类列表**（inspection-batches / repair-batches / part 批次明细）
//!   一律继续从 `current_process_step_id` → step JOIN 派生工序名。理由：
//!   `INSPECTION` 批次按出池不变式 `current_process_id` 恒为 NULL，直读会让
//!   这些端点的 `next_process_id` 恒 null（用户可见回归）。
//!
//! 5 条池 SQL 全部硬限定 `status='IN_PROCESS' AND location='PRODUCTION_SHELF'`
//! —— 这是「出池必须置 NULL」这条不变式的兜底，也是为什么残留脏值不会污染候选池。

use chrono::{NaiveDate, NaiveDateTime};

/// `t_part_batch` 行（Phase P1 投影；2026-09-16 PR-3 适配 step 化；
/// 2026-09-30 加 `current_process_id`）
///
/// 字段变化：
/// - `current_process_step_id: Option<i64>` —— 替代 `next_process_id`（已删），
///   逻辑 FK → `t_process_chain_step.id`；NULL = 批次尚未进入生产流或 part 无链
/// - `current_process_id: Option<i64>` —— 2026-09-30 新增，逻辑 FK →
///   `t_process.id`；工序池归属的**权威依据**
/// - `next_process_id` / `placed_at` 字段删除（t_part_batch 列已删）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPartBatch {
    pub id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String,
    pub location: Option<String>,
    pub current_holder_id: Option<i64>,
    /// 逻辑 FK → `t_process.id`；**工序候选池归属的权威依据**。
    ///
    /// 写入不变式（2026-09-30，2026-09-30 review 第 3 轮 M1 订正第 3 行）：
    /// - 进池（`status='IN_PROCESS'` + `location='PRODUCTION_SHELF'`）→ 目标 `process_id`
    /// - 出池（转 PENDING / INSPECTION / INSPECTION_SHELF）→ NULL
    /// - **工序推进**（worker-scan RETURNED：工人在 P1 完工、传
    ///   `next_process_id=P2`）→ 写 `Some(P2)`，批次落进**下一道**工序池。
    ///   ⚠️ 这条**也是**「worker → 生产架归还」，但语义是**推进工序**而非池内
    ///   移动，别与下一行混淆
    /// - **池内移动**（`move` 端点 POOL↔WORKER / WORKER↔WORKER；
    ///   `pick_up` 的 IN_PROCESS+PRODUCTION_SHELF 分支；`take_*_from_pool`
    ///   派发）→ **不动**
    /// - 非生产流（初始批次、子批次）→ NULL
    ///
    /// 唯一显式例外：`send_to_outsource` 写 `Some(req.process_id)`
    /// （`status='OUTSOURCE'` + `location='OUTSOURCE_COMPANY'`，池 SQL 硬限定
    /// IN_PROCESS + PRODUCTION_SHELF 故不可能命中；rollup 派生需要）。
    ///
    /// NULL = 批次不在生产工序池中（PENDING / PROGRAMMING / OUTSOURCE /
    /// INSPECTION / OFFICE 等）。
    ///
    /// **残留写点（2026-09-30 review 第 3 轮 M4，已接受债务）**：`cancel_batch` /
    /// `cancel_all_active_batches_for_part` / `force_complete_all_batches_for_part` /
    /// `mark_batch_repairing` 这 4 个出池写点既不写也不清本列。功能上无影响（5 条池
    /// SQL 全部 `status='IN_PROCESS' AND location='PRODUCTION_SHELF'` 双重限定）。
    /// 其中 `mark_batch_repairing`（IN_PROCESS → REPAIRING）**已决策**：
    /// 正常业务流不存在该转换，且 `REPAIRING` 将降级为纯标记（flag），
    /// 届时相关状态判定整体重做，本写点在**那次重构中一并处理**。
    /// 完整记录见 `migrations/20260930000000_004_add_batch_current_process_id.sql`
    /// 「已知局限 (4)」—— 做写点穷举时不必重新提这两条。
    pub current_process_id: Option<i64>,
    /// 逻辑 FK → `t_process_chain_step.id`；**可选的显示用定位信息**。
    ///
    /// 2026-09-30 review 第 3 轮订正措辞：本列**不是会随流转推进的「进度指针」**
    /// —— 它只在**首次定位**工序时被写入（dispatch 路径刻意写 NULL；其余由
    /// `place_on_shelf` / `release_from_programming` / `send_to_outsource` /
    /// `receive_from_outsource` / `complete_repair` / `to_process` 写），
    /// 之后**不再推进**：worker-scan RETURNED、send_to_inspection、
    /// `mark_batch_returned`、`mark_batch_inspected` 都不写它。对多工序链工单，
    /// 它永远停在首次定位的那一步，故**不可**当「当前走到第几步」用。
    ///
    /// NULL = 批次尚未进入生产流（PENDING/PROGRAMMING/OUTSOURCE 起点）、
    /// 所属 part 无工艺链，或 step 已软删。允许 NULL 是本设计的核心：池归属
    /// 判定已改由 `current_process_id` 承担，本列退化为可选的显示用信息。
    /// 2026-09-16 PR-3 替代 `next_process_id`（已删）。
    pub current_process_step_id: Option<i64>,
    pub delivery_note_id: Option<i64>,
    pub parent_batch_id: Option<i64>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// 草稿卡片「最近批次」展示行（`t_part_batch JOIN t_part` 投影）。
///
/// 2026-08-22 新增：配合 `ScanDeliveryNoteSummaryDto::recent_items` 返回。
/// 只投影卡片展示所需的 6 列（batch_id / part_id / serial_no / drawing_no /
/// name / order_no），比 `TPartBatch + TPart` 轻量。
#[derive(Debug, Clone)]
pub struct RecentBatchRow {
    pub batch_id: i64,
    pub part_id: i64,
    /// `t_part.serial_no` 是 nullable（手工工单可没序列号）。
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    /// `t_part.order_no` 是 nullable。
    pub order_no: Option<String>,
}

/// part-batches 端点的批次窄字段中间结构（service 内短暂使用，DTO 转换见 service）。
/// SQL 列别名见 repo `list_active_by_part_id_with_holder`。
#[derive(Debug, Clone)]
pub struct PartBatchScanRow {
    pub id: i64,
    pub quantity: i32,
    pub status: String,
    pub holder_name: Option<String>,
    pub version: i32,
}

// ===== Inspection Batch List =====

/// `GET /parts/inspection-batches` 单行中间结构（repo ↔ service 边界类型）。
///
/// SQL 列别名见 repo `list_batches_with_part`（单次 JOIN 8 表，含 holder_name
/// / next_process_name / delivery_note_no / customer_name / l1_customer_name
/// 全部解析）。
///
/// 2026-09-16 PR-3 批次 step 化：
/// - 删 `placed_at`（t_part_batch 列已删）
/// - `next_process_id` 改为派生：`LEFT JOIN t_process_chain_step s
///   ON s.id = pb.current_process_step_id` 后取 `s.process_id`，
///   `next_process_name` 由 `t_process np ON np.id = s.process_id` 拼齐
///
/// 2026-09-30（migration 004）一度改直读 `pb.current_process_id`，**2026-09-30
/// review 第 3 轮 M3 已回退**到 step 派生：INSPECTION 批次按出池不变式该列恒为
/// NULL，直读会让 `next_process_id` / `next_process_name` 在
/// `GET /parts/inspection-batches` 恒 null（用户可见回归）。字段名始终保留，
/// 兼容 DTO 与前端。
#[derive(Debug, Clone)]
pub struct InspectionBatchListRow {
    // 批次
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String,
    pub location: Option<String>,
    pub version: i32,
    /// 逻辑 FK → t_process_chain_step.id（2026-09-16 PR-3 替代 next_process_id）
    pub current_process_step_id: Option<i64>,
    pub parent_batch_id: Option<i64>,
    // holder / process / delivery_note 解析
    pub current_holder_id: Option<i64>,
    pub holder_name: Option<String>,
    /// 派生自 `current_process_step_id`（LEFT JOIN `t_process_chain_step` 取
    /// `s.process_id`）；保留字段名以兼容下游 DTO 与前端。
    ///
    /// **刻意不直读 `current_process_id`**：见本结构 doc 的 review 第 3 轮 M3 段。
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,
    pub delivery_note_id: Option<i64>,
    pub delivery_note_no: Option<String>,
    // 工单
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub order_no: Option<String>,
    pub planned_delivery_date: NaiveDate,
    pub is_urgent: bool,
    pub part_version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    // 客户
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}
