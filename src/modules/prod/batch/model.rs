//! prod::batch 的**窄投影行结构**（2026-10-08 起本文件只放投影，全列行已上移）
//!
//! `TPartBatch`（`t_part_batch` 全列行）已移到 `shared::batch::model`：它被十个域
//! 直接引用，属批次表的公共语义。本文件剩下的两个结构
//! （`RecentBatchRow` / `PartBatchScanRow`）都是**只服务 prod::batch 自己某条列表
//! 端点**的窄投影，故留在本域。
//!
//! 对应 Python myERP/model/part_batch.py。
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
//!   工序池的唯一权威依据**：queue 候选池 3 条 SQL + count 全部按本列
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
//!   1. `prod/inspection/repo.rs::InspectionQueueRepo::list_inspection_queue`
//!      —— `GET /prod/inspection/queue`（2026-10-07 自本域迁入 `prod::inspection`：
//!      3-JOIN 窄投影，**不投影** `next_process_*`，故本条已无「派生 vs 直读」之争；
//!      登记保留在此是因为本清单要覆盖**全仓**读点，而本条已是他域 SQL）
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
