//! prod::batch 子模块 VO —— 出参（handler 响应序列化层）
//!
//! 仅 `Serialize` 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//!
//! i64 一律走 `serialize_i64` → JSON string（雪花 ID > 2^53，JS `Number`
//! 会丢精度，参见 `shared::types` 模块 doc）。
//!
//! ## 分组
//! - **下发流**（`dispatch.rs`）：pending / dispatch / auto-dispatch 五类
//! - **流转与生命周期**（`transition*` / `lifecycle` / `shelf` / `programming` /
//!   `outsource` / `repair` / `batch_ops` / `pickup` / `scan` / `worker_scan`）：
//!   `ToXxxOut` / `BatchToXxxOut` / `BatchOpFailure` / `WorkerScanOut`
//! - **集合读**（`list.rs` / `repair.rs`）：`InspectionQueueListOut`
//!   （待品检队列，2026-10-03 VO 收口）+ `InspectionBatchListOut`
//!   （仅 repair / repairing 两条共用）
//!
//! 2026-10-02：to-XXX / batch-to-XXX / worker-scan / inspection·repair·repairing
//! 三条集合读这 8 类出参随批次用例自 `part::vo` 迁入。`PartOut` 例外 —— 它是
//! part 域实体投影，25 条路由与 part 域共用，本模块直接 `use`（单向依赖）。

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::modules::part::vo::PartOut;
use crate::modules::prod::batch::model::{InspectionBatchListRow, InspectionQueueRow};
use crate::modules::prod::worker_pool::model::RefillResult;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

// ===== pending list =====

/// `GET /api/v2/prod/batches/pending` 单条结构（车间 PENDING 批次 + 工单 + 客户
/// 解析 JOIN 后扁平投影）。
///
/// 字段顺序按业务语义分组：batch 标识 → 工单展示 → 客户 / 申请人 → 派生元数据
/// （is_urgent / version / step_id）。`planned_delivery_date` 是 `String` 而非
/// `NaiveDate` —— 即使 `t_part.planned_delivery_date` 为 NULL，service 也用
/// `"1970-01-01"` 兜底（与既有 list 端点惯例一致；DB `NOT NULL DEFAULT` 已保证
/// 字段非空，此兜底为防御性）。
#[derive(Debug, Clone, Serialize)]
pub struct PendingBatchItem {
    // —— batch 标识 ——
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// 工单序列号（手工工单可空）
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// 计划交期（`String` 而非 `NaiveDate`，NULL 走 `"1970-01-01"` 兜底）。
    pub planned_delivery_date: String,
    /// 系统交期（`NaiveDate` 原生序列化；NULL → JSON `null`）。
    pub system_delivery_date: Option<NaiveDate>,
    // —— 客户 / 申请人 ——
    /// L2 客户名（叶子）
    pub customer_name: Option<String>,
    /// L1 客户名（一级集团），L2.parent_id 为空时为 None
    pub parent_customer_name: Option<String>,
    /// 申请人字符串（来自 `t_part.applicant_name` LEFT JOIN `t_applicant.name`，
    /// applicant 软删 / 不存在时为 None）。
    pub applicant_name: Option<String>,
    // —— 派生元数据 ——
    pub is_urgent: bool,
    /// 工单级备注（`t_part.note`）
    pub note: Option<String>,
    pub version: i32,
    /// `current_process_step_id`（PENDING 时通常 NULL；PART 自动下发时
    /// 已写入首道 step）。
    ///
    /// **NULL 兜底语义**：DB 列 NULL 时 row → vo 投影为 0（`Option<i64> → i64`
    /// 走 `.unwrap_or(0)`）；前端按 `0 == "未设 step"`、`> 0 == "已设 step"`
    /// 区分。dispatch 路径写入 batch 时显式 `NULL`（见
    /// `BatchRepo::update_batch_dispatched`），符合「PENDING 尚未挂 step」语义。
    /// 注：本字段未走 `Option<i64>` 是为了对齐本 VO 整体扁平数字风格（与
    /// `process_chain_id` 同形态）；语义差异由前端按 status 区分。
    #[serde(serialize_with = "serialize_i64")]
    pub current_process_step_id: i64,
    /// `t_part.process_chain_id`（PR-3 step 化后新字段；PENDING 列表透传
    /// 给前端做 UI 关联）。
    #[serde(serialize_with = "serialize_i64")]
    pub process_chain_id: i64,
}

/// `GET /api/v2/prod/batches/pending` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct PendingBatchListOut {
    pub items: Vec<PendingBatchItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ===== dispatch result =====

/// `POST /api/v2/prod/batches/dispatch` 单条结果（bulk-only：单条下发即
/// `succeeded.len() == 1`）。
///
/// 2026-09-30 重构：原 `DispatchResult`（单条）+ `BulkDispatchResult`（succeeded/failed）
/// 合并为统一 bulk 形态 `DispatchResult { succeeded }`：
/// - 单条下发 = 1 元素 succeeded
/// - 多批下发 = N 元素 succeeded（全成功）或 service 抛 AppError（任一硬错误全回滚，
///   响应为顶层 4xx/5xx，failed 数组废弃）
///
/// 当前实现走「任一失败 → 全回滚」语义；`failed` 字段保留为 `Vec<DispatchFailureItem>`
/// 是为未来启用 partial commit 时向前兼容，**当前总是空**。
///
/// `current_process_step_id` 是 `Option<i64>`（dispatch 路径不解析 step，存 NULL）。
/// `current_process_id`（2026-09-30 新增）是 `Option<i64>`，值恒为
/// `Some(target_process_id)` —— 池归属的权威依据。
#[derive(Debug, Clone, Serialize)]
pub struct DispatchResult {
    /// 成功下发的 batch 列表（顺序与 req.targets 一致）。
    pub succeeded: Vec<DispatchSuccessItem>,
    /// 失败明细（当前总为空；预留 partial commit 启用）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<DispatchFailureItem>,
}

/// `DispatchResult.succeeded` 单条（每条 target 对应一个）。
///
/// 2026-09-30 新增 `current_process_id`（工序池归属权威依据）；原
/// `current_process_step_id` 走 `Option<i64>`（dispatch 不解析 step → None）。
#[derive(Debug, Clone, Serialize)]
pub struct DispatchSuccessItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub current_process_step_id: Option<i64>,
    /// 下发后写入 `t_part_batch.current_process_id` 的值（逻辑 FK → `t_process.id`），
    /// 恒等于本次 `target_process_id`。
    ///
    /// **Option 语义**：当前 dispatch 路径恒为 `Some(target_process_id)`；保留
    /// `Option` 是为了与 `current_process_step_id` 对齐并为将来「下发不到指定
    /// 工序」的分支留出 `null` 表达。None → JSON `null`，避免前端拿 `"0"` 误判
    /// 为合法工序 id。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_process_id: Option<i64>,
    #[serde(serialize_with = "serialize_i64")]
    pub target_process_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub version: i32,
}

/// `DispatchResult.failed` 单条（当前总为空；为 partial commit 启用预留）。
///
/// 注：本类型当前未被任何 service 代码生成，但保留作为 VO schema 的稳定部分；
/// 未来 partial commit 启用时，service 在每条失败处 push `DispatchFailureItem` 而非抛错。
#[derive(Debug, Clone, Serialize)]
pub struct DispatchFailureItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub code: i32,
    pub message: String,
}

// ===== auto-dispatch preview (2026-09-30 重构为只读查询) =====

/// `POST /api/v2/prod/batches/auto-dispatch` 单条预览项。
///
/// 2026-09-30 重构：原 `auto_dispatch` 改为只读 `auto_dispatch_preview`，
/// 不再真正下发批次，仅返回每个 batch 的「首道工序 + 首货架」+ skip_reason。
/// 实际下发仍走 `POST /api/v2/prod/batches/dispatch`，caller 据此构造
/// `targets: [{batch_id, target_process_id}]` 发起真正下发。
///
/// 字段语义：
/// - `batch_id` / `part_id` —— 必填
/// - `process_chain_id` / `first_process_id` / `first_process_code` / `first_process_name`
///   —— 当 `skip_reason = NO_PROCESS_CHAIN / NO_PROCESS_STEP` 时为 None
/// - `first_shelf_id` —— 当首道工序未映射货架时为 None（skip_reason=NO_SHELF）
/// - `skip_reason` —— NOT_FOUND / NO_PROCESS_CHAIN / NO_PROCESS_STEP / NO_SHELF 之一
///   或 None（一切就绪可下发）
#[derive(Debug, Clone, Serialize)]
pub struct AutoDispatchItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// `t_part.process_chain_id`（PR-3 step 化后新字段）
    ///
    /// **Option 语义**：当 `skip_reason = NO_PROCESS_CHAIN` 时为 None；其余情形透传 part 实际
    /// 值（即便其它上层查不到也保持原值不动——避免误导 frontend）。None → JSON `null`，避免
    /// 前端拿 `"0"` 误判为合法 chain。
    ///
    /// 2026-09-30 review 第 1 轮：原为 `i64 + serialize_i64` + service `unwrap_or(0)` 兜底，
    /// 导致 NO_PROCESS_CHAIN 时输出 `"process_chain_id": "0"`；改为 Option 与 plan §3.2 对齐。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub process_chain_id: Option<i64>,
    /// 首道工序 id（`t_process_chain_step` sort_order=1 行的 process_id）
    ///
    /// **Option 语义**：当 `skip_reason = NO_PROCESS_CHAIN / NO_PROCESS_STEP` 时为 None；
    /// OK 时为 Some(process_id)；NO_SHELF 时仍 Some（首道工序存在但未映射货架）。
    /// None → JSON `null`。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub first_process_id: Option<i64>,
    pub first_process_code: String,
    pub first_process_name: String,
    /// 首道工序对应的候选货架（按 `t_shelf_process.sort_order ASC`）
    ///
    /// **Option 语义**：当 `skip_reason = NO_PROCESS_CHAIN / NO_PROCESS_STEP / NO_SHELF`
    /// 时为 None；其余情形为 Some(shelf_id)。None → JSON `null`。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub first_shelf_id: Option<i64>,
    /// 取不到任一上游数据时的原因：NOT_FOUND / NO_PROCESS_CHAIN / NO_PROCESS_STEP / NO_SHELF
    /// （OK 时为 None）
    pub skip_reason: Option<String>,
}

/// `POST /api/v2/prod/batches/auto-dispatch` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct AutoDispatchResult {
    pub items: Vec<AutoDispatchItem>,
}

// ===== 集合读（待品检队列：inspection 专用）=====

/// `GET /api/v2/prod/batches/inspection` 出参项。
///
/// 2026-10-03 VO 收口：本 VO 只服务待品检队列页，字段严格对齐前端 7 个数据列
/// （序列号 / 图号 / 名称 / 批次 / 数量 / 系统交期 / 客户）+ 操作列所需的
/// 锚点（`batch_id` / `version` / `part_id` / `is_urgent` / `customer_id`）。
/// 返修两条端点继续用 [`InspectionBatchListItemOut`]（28 字段，本 VO 不共用）——
/// 共用会让那 15 个字段在待品检页成为无用负载。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionQueueItemOut {
    /// 三个写端点的路径参数 + 扫码选择行标识。
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// 批次列。
    pub batch_no: i32,
    /// 数量列 + 部分通过弹窗上限（`POST /prod/batches/{batch_id}/to-ship` 的
    /// `quantity` 不得超过本值）。
    pub quantity: i32,
    /// OCC 锚 `t_part_batch.version`（**不是** `t_part.version`）。
    pub version: i32,
    /// 详情页 `/parts/{part_id}`。
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// 序列号列（`t_part.serial_no` 可空：手工工单可没序列号）。
    pub serial_no: Option<String>,
    /// 图号列。
    pub drawing_no: String,
    /// 名称列。
    pub name: String,
    /// 系统交期列（2026-10-03 新增投影）。可空 → JSON `null`。
    pub system_delivery_date: Option<NaiveDate>,
    /// 加急红底。
    pub is_urgent: bool,
    /// 客户表头筛选的入参回显（caller 选中 L1 / L2 都用它）。
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

impl From<InspectionQueueRow> for InspectionQueueItemOut {
    fn from(r: InspectionQueueRow) -> Self {
        Self {
            batch_id: r.batch_id,
            batch_no: r.batch_no,
            quantity: r.quantity,
            version: r.version,
            part_id: r.part_id,
            serial_no: r.serial_no,
            drawing_no: r.drawing_no,
            name: r.name,
            system_delivery_date: r.system_delivery_date,
            is_urgent: r.is_urgent,
            customer_id: r.customer_id,
            customer_name: r.customer_name,
            l1_customer_name: r.l1_customer_name,
        }
    }
}

/// `GET /api/v2/prod/batches/inspection` 出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionQueueListOut {
    pub items: Vec<InspectionQueueItemOut>,
    #[serde(serialize_with = "serialize_i64")]
    pub total: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub limit: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub offset: i64,
}

// ===== 集合读（返修：repair / repairing 两条共用）=====

/// `GET /prod/batches/repair` / `repairing` 列表行：批次 + 工单 + 客户 + holder/
/// process/delivery_note 名称（一次性 JOIN 解析，不在 service 做 N+1）。
///
/// **仅这 2 条端点共用**（2026-10-03 起）：`GET /prod/batches/inspection` 已切到
/// 精简 VO [`InspectionQueueItemOut`]（13 字段）—— 待品检页只渲染 7 个数据列，
/// 共用本 VO 会让 15 个字段成为无用负载。
///
/// 字段命名沿用 v1 `PartOut`/`PartBatchOut` 约定（`batch_id` 即 `t_part_batch.id`，
/// `version` 即乐观锁版本号）。前端用 `batch_id + version` 直接拼
/// `POST /prod/batches/{batch_id}/to-ship` 或 `to-inspection` 的请求体。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `has_been_repaired` 字段
/// （t_part_batch 列已删；返修事实由 t_part_event REPAIR_STARTED 事件追溯）。
///
/// 2026-09-16 PR-3 批次 step 化（migration 028）：
/// - 删 `placed_at`（t_part_batch 列已删，不再统计生产时间）
/// - 新增 `current_process_step_id`：逻辑 FK → t_process_chain_step.id
///   （批次**首次定位**的工艺链步骤；NULL = 批次尚未进入生产流或 part 无链）
/// - `next_process_id` / `next_process_name` 字段保留，由 repo JOIN step 派生
///   （保持 DTO 兼容，不破坏前端）
///
/// 2026-09-30（review 第 3 轮 M3）：本 VO 的 `next_process_id` 保持**从
/// `current_process_step_id` 经 step JOIN 派生**，不改直读新列
/// `current_process_id`。后者是**池归属权威列**，只服务 5 条工序池 SQL + rollup；
/// INSPECTION 批次按出池不变式该列恒为 NULL，直读会让本 VO 的工序字段恒 null。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionBatchListItemOut {
    // ===== 批次字段 =====
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String, // 必为 "INSPECTION"
    /// 2026-10-01 review 第 1 轮 M5 新增（migration 005，**BREAKING**）：
    /// `REPAIRING` 已从 `PartStatus` 降级为 `t_part_batch.is_repairing` 标记列，
    /// 本 VO 的 `status` 因此**恒为** `IN_PROCESS`（`GET /prod/batches/repairing`
    /// 的过滤判据已是 `is_repairing = true`）。改造前前端靠
    /// `status === 'REPAIRING'` 标「返修中」，现在任何端点都拿不到该值 ——
    /// 除非读本字段。
    pub is_repairing: bool,
    pub location: Option<String>,
    pub version: i32,
    /// 逻辑 FK → t_process_chain_step.id（2026-09-16 PR-3；替代 next_process_id 列）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_process_step_id: Option<i64>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub parent_batch_id: Option<i64>,

    // ===== holder 解析（COALESCE 三表）=====
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_holder_id: Option<i64>,
    pub holder_name: Option<String>,
    /// 由 `current_process_step_id` 经 `LEFT JOIN t_process_chain_step` 取
    /// `s.process_id` 派生；保留字段名以兼容前端契约。
    ///
    /// 2026-09-30 一度改直读 `t_part_batch.current_process_id`（migration 004），
    /// **2026-09-30 review 第 3 轮 M3 已回退**（follow-up 于 2026-09-30 补齐
    /// repair 两端点的漏网项）。原因：送检 = 出池，该列被置 NULL。
    ///
    /// 本字段由 2 个端点共用（`InspectionBatchListItemOut`，别名
    /// `RepairBatchesOut`），**2 个都不是工序池端点**：
    /// - `GET /prod/batches/repair`（`DELIVERED`，2026-09-30 follow-up 补齐）
    /// - `GET /prod/batches/repairing`（`is_repairing = true`，2026-10-01 由
    ///   `status='REPAIRING'` 改为标记列过滤）
    ///
    /// DELIVERED 端点的 `current_process_id` 按「DELIVERED 必经 INSPECTION」
    /// 不变式同样恒 NULL（进 INSPECTION 的写点都把该列清成 NULL）⇒ 直读得不到
    /// 正确值，故该查询走 step 派生。
    /// 返修中端点：2026-10-01 起 `mark_batch_repairing` **不再把批次
    /// 翻出 IN_PROCESS**（status 保持不变），故该列对返修批次不再有「残留
    /// 陈旧值」问题 —— 但本 VO 仍统一走 step 派生，理由是「展示类列表一律走
    /// step 派生」这条分工（见 `prod/batch/model.rs` 模块 doc 的读取方清单），
    /// 不因单个端点的判据变化而分叉。
    /// `current_process_step_id` 在送检期间被刻意保留
    /// （`mark_batch_inspected` 不写它），正是「INSPECTION 期间显示批次走到
    /// 工艺链第几步」这条产品需求的数据来源。
    ///
    /// 2026-09-27 part 域前后端字段对齐：`/parts` 响应已对该字段加
    /// `#[serde(skip)]` 仅隐藏（DB 列保留、rollup 派生链路不变）。repair 域
    /// **不做 skip**，字段照常序列化。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,

    // ===== delivery_note 解析 =====
    #[serde(serialize_with = "serialize_i64_opt")]
    pub delivery_note_id: Option<i64>,
    pub delivery_note_no: Option<String>,

    // ===== 工单字段（JOIN t_part）=====
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub order_no: Option<String>,
    pub planned_delivery_date: NaiveDate,
    pub is_urgent: bool,
    pub part_version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,

    // ===== 客户解析（JOIN t_customer + 自连 L1）=====
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

impl From<InspectionBatchListRow> for InspectionBatchListItemOut {
    fn from(r: InspectionBatchListRow) -> Self {
        Self {
            batch_id: r.batch_id,
            batch_no: r.batch_no,
            quantity: r.quantity,
            status: r.status,
            is_repairing: r.is_repairing,
            location: r.location,
            version: r.version,
            current_process_step_id: r.current_process_step_id,
            parent_batch_id: r.parent_batch_id,
            current_holder_id: r.current_holder_id,
            holder_name: r.holder_name,
            next_process_id: r.next_process_id,
            next_process_name: r.next_process_name,
            delivery_note_id: r.delivery_note_id,
            delivery_note_no: r.delivery_note_no,
            part_id: r.part_id,
            serial_no: r.serial_no,
            drawing_no: r.drawing_no,
            name: r.name,
            order_no: r.order_no,
            planned_delivery_date: r.planned_delivery_date,
            is_urgent: r.is_urgent,
            part_version: r.part_version,
            created_at: r.created_at,
            updated_at: r.updated_at,
            customer_id: r.customer_id,
            customer_name: r.customer_name,
            l1_customer_name: r.l1_customer_name,
        }
    }
}

/// `GET /api/v2/prod/batches/repair` / `repairing` 出参（分页）：返修批次列表。
///
/// **仅这 2 条端点**（2026-10-03 起）：`GET /prod/batches/inspection` 已切到
/// [`InspectionQueueListOut`]，两条端点只有过滤判据不同。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionBatchListOut {
    pub items: Vec<InspectionBatchListItemOut>,
    #[serde(serialize_with = "serialize_i64")]
    pub total: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub limit: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub offset: i64,
}

/// `GET /api/v2/prod/batches/repair` / `repairing` 出参别名。
pub type RepairBatchesOut = InspectionBatchListOut;

/// 单件 / 批量 to-XXX 端点的统一出参 shape。
///
/// `part`：操作后 part 的最新 [`PartOut`] 投影（含 OCC 更新后的 `version`）。
/// `new_batch_id`：仅当 `quantity < target.quantity` 走拆批分支时为
///   `Some(remainder_id)`（拆批后**剩余批次**的 id，留在源状态待后续操作）；
///   整批操作时为 `None`（序列化为 JSON `null`），前端拿到非 null 时应刷新批次列表。
///   用 `serialize_i64_opt` 把 Some 序列化为 JSON 字符串、None 序列化为 `null`，
///   跟 [`PartOut`] 的雪花 id 序列化契约对齐。
/// `synced_assembly_id`：仅当本 part 由 inspection 流触发父装配件 status 翻转时
///   为 `Some(assembly_id)`（handler 据此发 `ASSEMBLY_UPDATED` WS 广播）；
///   无父装配件或父未变更时为 `None`。
#[derive(Debug, Clone, Serialize)]
// ===== to-XXX 三流 / 批量流转 / worker-scan =====

pub struct ToXxxOut {
    pub part: PartOut,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub new_batch_id: Option<i64>,
    /// 父装配件 id（仅当本 part 由 inspection 流触发父 status 变更时 Some）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub synced_assembly_id: Option<i64>,
}

/// Per-item 失败明细（item 级别错误，非整批失败）。
///
/// `batch_id`：按 batch 定位失败 item（批量 item 不含 `part_id`，服务从
///   `BatchOpItem::batch_id` 反查后回填；无法 parse 的串落到 `40001` 失败，
///   不进入本结构）。`i64` 而非 `String` 是因为 service 已 parse 过一次，
///   用 `serialize_i64` 序列化为 JSON 字符串与前端 batch_id 字段类型对称。
/// `code` 透传 service 层错误码（20103 / 20104 / 20109 / 20111 / 20511 / 20512 / 40901）；
/// `message` 透传 service 层错误文案（前端可作 toast）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchOpFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub code: i32,
    pub message: String,
}

/// 批量端点统一出参（`batch-to-ship` / `batch-to-inspection` 共用）。
///
/// `submitted`：成功并完成状态流转的 item（含 `PartOut` 最小投影 + 拆批后的
///   `new_batch_id`）；`failed`：item 级别错误（共享 [`BatchOpFailure`]）。
/// `submitted` 与 `failed` 互斥，单 item 不会同时出现在两侧。
#[derive(Debug, Clone, Serialize)]
pub struct BatchToXxxOut {
    pub submitted: Vec<ToXxxOut>,
    pub failed: Vec<BatchOpFailure>,
}

/// worker-scan 核心出参（不含 refill）。
///
/// handler 会把 `scan + refill` 一起装到 [`WorkerScanOut`] 返回；
/// `WorkerScanCoreOut` 是 service 层直接产出的最小投影（与 worker-pool
/// `RefillResult` 解耦，便于 service 层单测）。
///
/// `work_type_id` 与 `badge_code` 是**内部管道字段**：handler 用它把
/// `worker_scan_event` 已经 fetch 过的 worker 信息透传给同事务的
/// `WorkerPoolService::refill_for_worker_with_work_type`，避免重复
/// `WorkerRepo::get_by_id` 查询。不暴露到 JSON 响应里。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerScanCoreOut {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub event_type: String,
    /// 父装配件 id（仅当 INSPECTED 分支触发父 status 变更时 Some）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub synced_assembly_id: Option<i64>,
    /// 内部：透传给 refill，refill 不再 fetch worker。
    #[serde(skip)]
    pub work_type_id: i64,
    /// 内部：refill 写 `TAKEN_FROM_POOL` 事件日志需要 badge_code。
    #[serde(skip)]
    pub badge_code: String,
}

/// worker-scan 端点出参：`scan` + 同事务 refill 结果。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerScanOut {
    pub scan: WorkerScanCoreOut,
    pub refill: RefillResult,
}
