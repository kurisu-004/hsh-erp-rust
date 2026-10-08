//! 跨域批次行模型（2026-10-08 自 `prod::batch::model` 抽出）
//!
//! 本文件只放 `TPartBatch` —— 全仓唯一的 `t_part_batch` 全列行结构。
//! 它被 prod::batch / prod::queue / part / delivery_note / outsource / wx /
//! statistics / admin / dashboard 十个域直接引用，任何一个字段语义的变化都会
//! 同时影响这些域的响应，故放 shared 而非某个域。
//!
//! `prod::batch::model` 里的其余行结构（`RecentBatchRow` / `PartBatchScanRow` /
//! `InspectionQueueRow`）都是**投影窄行**、只服务本域自己的列表端点，仍留在
//! `prod::batch::model`。
//!
//! ## 字段读取分工（勿越界）
//!
//! - `current_process_id` 的**工序池类**读取方严格限定为 5 条工序池 SQL
//!   （take_one / take_specific / list_candidates / group_count /
//!   count_pool_by_shelf）与 `list_pickable_by_work_type`，外加 rollup 派生
//!   `t_part.next_process_id`；工序池口径之外另有 1 处有意例外：扫码树
//!   （`prod::inspection::repo::InspectionScanRepo::list_batches_by_part_ids`）
//!   直读本列，理由见该方法 doc。
//! - **展示类列表一律继续从 `current_process_step_id` → step JOIN 派生工序名**，
//!   完整清单与取舍理由见 `prod::batch::model` 模块 doc。
//! - 5 条池 SQL 全部硬限定 `status='IN_PROCESS' AND location='PRODUCTION_SHELF'`
//!   —— 这是「出池必须置 NULL」这条不变式的兜底，也是为什么残留脏值不会污染候选池。

use chrono::NaiveDateTime;

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
    /// 唯一显式例外：外协发送（`outsource` 域 `service/move.rs` 的
    /// `PRODUCTION_SHELF → OUTSOURCE_COMPANY` 臂）写 `Some(batch.current_process_id)`
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
    /// `place_on_shelf` / `release_from_programming` / 外协收发（`outsource` 域
    /// `service/move.rs`）/ `complete_repair` / `to_process` 写），
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
    /// 2026-10-10 新增（migration `20261010000000_001_add_batch_delivery_seq`）：
    /// **加入当前送货单的次序**，本单内从 1 起递增（可能不连续：摘单只清被摘行、
    /// 不重排剩余行，详见 `PartBatchRepo::list_with_part_by_delivery_note`）。
    ///
    /// **NULL = 未挂单**（与 `delivery_note_id IS NULL` 同步：挂单写点赋值、
    /// 摘单 / 单据软删写点置 NULL）。本列**只在「这一张送货单」的语境内有意义**：
    /// 同一个批次先后挂过两张单，`delivery_seq` 是相对各自那张单重新计的，
    /// 拿它跨单比较无意义。
    ///
    /// 为什么不用批次 id 当排序键：id 是**建批顺序**，只有拆批路径产生新 id
    /// （反映扫码时刻），整批直接挂单的批次用的是建批时的 id ⇒「先扫 A 后扫 B、
    /// 但 A 建得更早」会把 A 排前面。送货单详情的零件列表要的是扫码次序。
    ///
    /// 与本仓 `sort_order`（iam 菜单 / 外协公司等配置表的显示序）语义不同，
    /// 不要混用：本列是挂单时自动递增的**业务事实**，`sort_order` 是人工维护的
    /// **配置显示序**。
    pub delivery_seq: Option<i64>,
    pub parent_batch_id: Option<i64>,
    /// 2026-10-01 新增（migration 005）：本批次**当前**是否处于返修中。
    ///
    /// REPAIRING 已从 `PartStatus` 降级为标记（flag），`status` 保持
    /// `IN_PROCESS`（返修仍在生产中，progress 与 IN_PROCESS 同档）；
    /// 返修事实改由本列承载。写入路径**唯一**：`shared::batch::status::
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
