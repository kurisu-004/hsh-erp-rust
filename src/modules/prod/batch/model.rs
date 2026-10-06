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
//!   （只在首次定位工序时写、之后不再推进，详见 `TPartBatch` 字段 doc）
//! - 目的：让**没有工序链的工单，其批次也能正常入池**（旧设计下 dispatch 写
//!   `step=NULL` + 候选池 SQL INNER JOIN step → 批次对所有池查询隐身，形成
//!   「要推进 step 先进池、要进池先有 step」的死状态）
//!
//! **读取方分工**（勿越界）：
//!
//! - `current_process_id` 的**工序池类**读取方严格限定为 **5 条工序池 SQL**（take_one /
//!   take_specific / list_candidates / group_count / count_pool_by_shelf）与
//!   `list_pickable_by_work_type`，外加 rollup 派生 `t_part.next_process_id`；
//!   工序池口径之外另有 1 处有意例外（见下方第 4 条）。
//! - **展示类列表一律继续从 `current_process_step_id` → step JOIN 派生工序名**，
//!   唯一**有意例外**是扫码树（第 4 条，已在下方登记）。完整清单（改动前请逐条
//!   对照，勿凭端点名想当然）：
//!   1. `prod/batch/repo/list.rs::list_inspection_queue`
//!      —— `GET /prod/batches/inspection`（2026-10-03 起 3-JOIN 窄投影，
//!      **不投影** `next_process_*`，故本条已无「派生 vs 直读」之争）
//!   2. `prod/batch/service/repair.rs::list_batches_matching`
//!      —— `GET /prod/batches/repair`（DELIVERED）+ `GET /prod/batches/repairing`
//!      （`is_repairing = true`）。
//!   3. `part/service/phase1/lifecycle_helpers.rs::list_batches`
//!      —— `GET /parts/{id}/batches`（工单批次明细，**无 status 过滤**，同时返回
//!      PENDING / IN_PROCESS / INSPECTION / READY_TO_SHIP 等各状态批次；返修中的
//!      批次按 `IN_PROCESS` 一并返回）
//!   4. ⚠️ **有意例外**：`prod/inspection/repo.rs::InspectionScanRepo::
//!      list_batches_by_part_ids` —— `GET /prod/inspection/scan/{serial_no}`
//!      （2026-10-05 新增的扫码树，批次层唯一一条 SQL）。它的 `process_name`
//!      直读 `current_process_id`，与第 1~3 条**故意不同**，理由是本端点的
//!      工序名要回答「这批货现在在哪道工序」，而 step 指针只在首次定位工序时写、
//!      之后永不推进，多工序链工单上会停在第一步 → 用它渲染会显示过时工序。
//!      代价是本端点的 `INSPECTION` / `DELIVERED` 批次 `process_name` 恒 `null`
//!      （这两个状态本就不在生产流里，不渲染工序标签即可）。
//!
//!   理由（第 1~3 条的推导）：上述端点都不是**工序池**端点，判据是 `status`，与
//!   `current_process_id` 无关；而 `INSPECTION` 批次按出池不变式该列恒为 NULL
//!   （DELIVERED 更进一步 —— 进 `READY_TO_SHIP` 的边只有 `INSPECTION →
//!   READY_TO_SHIP`，故 DELIVERED 批次也**必经 INSPECTION**、该列同样恒 NULL），
//!   直读会让这些端点的 `next_process_id` / `next_process_name` 结构性恒 null
//!   （用户可见回归）。
//!
//! - ⚠️ **dashboard 不在上条清单内，也不需要进清单**（2026-10-07 复核）：它现在只剩
//!   2 条相关查询，两条都碰不到「派生 vs 直读」这个选择：
//!   - `dashboard/repo/sql.rs::fetch_worker_rows`
//!     （`status='IN_PROCESS' AND location='WORKER'`）—— 判据是**位置**
//!     （压在工人手上），不投影任何工序字段，无从选择；
//!   - `dashboard/repo/sql.rs::count_inspection_batches`
//!     （`status='INSPECTION'` + holder 在品检区 active 货架）—— 只 `COUNT(*)`，
//!     不返回行内容，同样没有工序名可渲染。
//!     改这两条查询时若新增了工序字段投影，先回到本段重新判断该走 step 派生还是直读。
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
    /// `cancel_all_active_batches_for_part` / `force_complete_all_batches_for_part`
    /// 这 3 个出池写点既不写也不清本列。功能上无影响（5 条池
    /// SQL 全部 `status='IN_PROCESS' AND location='PRODUCTION_SHELF'` 双重限定）。
    /// 第 4 个写点 `mark_batch_repairing` **已于 2026-10-01 消解**：REPAIRING
    /// 降级为 `is_repairing` 标记列后，它不再把批次翻出 `IN_PROCESS`
    /// （status 保持不变），于是「该清未清」的问题不复存在 ——
    /// 返修中的批次继续留在原工序池中（工人才可能把它领走去修）。
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
    /// 2026-10-01 新增（migration 005）：本批次**当前**是否处于返修中。
    ///
    /// REPAIRING 已从 `PartStatus` 降级为标记（flag），`status` 保持
    /// `IN_PROCESS`（返修仍在生产中，progress 与 IN_PROCESS 同档）；
    /// 返修事实改由本列承载。写入路径**唯一**：`service::status_gate::
    /// apply_batch_status_change`（`is_repairing: Some(bool)`），caller 无
    /// 「要不要顺手写一下」的选择权。
    ///
    /// 与 2026-09-16 已删的 `has_been_repaired` 区别：那是「**曾经**返修过」
    /// （历史事实，拆批后失真而废弃），本列是「**当前**返修中」（当前态，
    /// 批次粒度，不受拆批影响）。
    pub is_repairing: bool,
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

// ===== Inspection Queue =====

/// `GET /prod/batches/inspection` 单行中间结构（repo ↔ service 边界类型）。
///
/// 2026-10-03 VO 收口新增：待品检页只渲染 7 个数据列（序列号 / 图号 / 名称 /
/// 批次 / 数量 / 系统交期 / 客户）。同批删掉原 28 字段宽投影
/// `InspectionBatchListRow` —— 待品检端点是它在 prod 域的最后调用方；返修两条
/// 端点（`/repair` / `/repairing`）在 service 层直接构造
/// `vo::InspectionBatchListItemOut`，不经 repo 行结构。
///
/// 字段与 `vo::InspectionQueueItemOut` 逐字同形（13 个）：SQL 侧列别名直接取
/// 语义名（`pb.id AS batch_id` 等），repo 层 1:1 搬运，service 只做形状转换。
/// `l1_customer_name` 的派生在 repo 层完成（原料列 `c.parent_id` / `pc.name`）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InspectionQueueRow {
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// OCC 锚 `t_part_batch.version`（不是 `t_part.version`）。
    pub version: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    /// 系统交期（2026-10-03 新增投影；页面已不显示计划交期，日期筛选改筛本列）。
    pub system_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}
