use serde::{Deserialize, Serialize};
use crate::shared::types::{deserialize_i64, deserialize_i64_opt, serialize_i64};

use super::model::TakenItem;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WorkerScanEvent {
    RETURNED,
    INSPECTED,
}

/// POST /api/v2/admin/worker-pool/refill
#[derive(Debug, Clone, Deserialize)]
pub struct AdminRefillRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
}

/// POST /api/v2/admin/worker-pool/remove —— 把指定 batch 从 worker 持有中按 RETURNED 语义放回 pool
#[derive(Debug, Clone, Deserialize)]
pub struct AdminRemoveRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub next_process_id: i64,
}

// 追加到 dto.rs 末尾（保持现有 `serialize_i64` 风格）

/// `GET /api/v2/worker-pool/{process_id}` —— 单条候选批次卡片。
///
/// 字段顺序：`batch_id → part_id → 业务字段 → version`，与既有 `TakenItem` 一致。
/// 多处字段源自 `t_part`（不在 `t_part_batch`）：`name / drawing_no / serial_no /
/// system_delivery_date / is_urgent / note / customer_id / applicant_name`。
#[derive(Debug, Clone, Serialize)]
pub struct PoolBatchItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// 工单序列号（手工工单可空）
    pub serial_no: Option<String>,
    /// 工单 / 零件名称
    pub name: String,
    pub drawing_no: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    /// L2 客户名（叶子）
    pub customer_name: Option<String>,
    /// L1 客户名（一级集团），L2.parent_id 为空时为 None
    pub parent_customer_name: Option<String>,
    /// `"L1 / L2"` 路径；L1 自指仅给 leaf
    pub customer_path: Option<String>,
    /// 申请人字符串列（t_part.applicant_name，非 FK）
    pub applicant_name: Option<String>,
    /// 候选池当前货架 raw enum（如 `"PRODUCTION_SHELF"`）
    pub location: String,
    /// 当前货架 id（t_part_batch.current_holder_id）
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub shelf_code: String,
    pub shelf_name: String,
    /// 是否加急（取自 t_part.is_urgent）
    pub is_urgent: bool,
    /// 工单级备注（t_part.note，DB 无 batch 级 remark 字段；复用）
    pub note: Option<String>,
    // 2026-09-16 PR-3 批次 step 化：删 `placed_at`（t_part_batch 列已删）
    // —— 前端如需展示积压时间，由前端按 PICKED_UP 事件 created_at 自派生
    pub version: i32,
}

/// 「可执行该工序的工人」单条记录（来自 t_worker JOIN t_work_type_process）。
///
/// 同一工人可能因所属工种映射该工序而出现一次；带 `work_type_id / work_type_code`
/// 让前端按工种分组展示。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerBrief {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    pub name: String,
    #[serde(serialize_with = "serialize_i64")]
    pub work_type_id: i64,
    pub work_type_code: String,
}

/// 「该工序映射到的工种 + max_held」一条记录。
///
/// 同一 process 可被多个 work_type 映射，每 work_type 自己的
/// `max_held_batches` 可能不同——按用户决定按 work_type 分组返回。
/// `max_held_batches = None` 表示工种 max_held 未设置（与既有 20904
/// `BIZ_WORK_TYPE_MAX_HELD_NOT_SET` 同语义，但不在此处报错）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeMaxHeld {
    #[serde(serialize_with = "serialize_i64")]
    pub work_type_id: i64,
    pub work_type_code: String,
    pub work_type_name: String,
    pub max_held_batches: Option<i32>,
}

/// `GET /api/v2/worker-pool/{process_id}` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessPoolDetail {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub process_name: String,
    pub workers: Vec<WorkerBrief>,
    pub work_types: Vec<WorkTypeMaxHeld>,
    /// 候选批次总数（与 items.len() 一致，不分页；admin 视角全量）
    pub total: i64,
    pub items: Vec<PoolBatchItem>,
}

// ---------------------------------------------------------------------------
// auto-allocate 端点（part-worker-pool-federated-rocket 2026-09-11 新增）
// ---------------------------------------------------------------------------

/// 自动分配模式：`COUNT` 按批次数填满；`TIME` 按累计预估工时填满。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum AutoAllocateMode {
    Count,
    Time,
}

/// `POST /api/v2/admin/worker-pool/auto-allocate`
///
/// 按 process + shelf 范围为每个匹配 worker 计算 `target` 并循环 refill。
/// - `fill_ratio` 必须在 `[0.0, 1.0]`，否则 `20704 BIZ_AUTO_ALLOCATE_INVALID_RATIO`
/// - `mode=COUNT`：`target = ceil(work_type.max_held_batches × fill_ratio)`；
///   work_type.max_held_batches IS NULL → `20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET`
/// - `mode=TIME`：`target = ceil(work_type.max_held_minutes × fill_ratio)`；
///   work_type.max_held_minutes IS NULL → `20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET`
#[derive(Debug, Clone, Deserialize)]
pub struct AutoAllocateRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub process_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    pub mode: AutoAllocateMode,
    pub fill_ratio: f64,
}

/// 单个 worker 的填充结果。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerFillItem {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    /// 该 worker 的目标（按 mode 计算）
    pub target: i32,
    /// 实际抢到的批次 / 累计分钟数（按 mode 解释：COUNT=抢到的批次数；TIME=抢到的累计工时）
    pub filled_count: i32,
    /// 是否因业务错（如 max_held_batches/max_held_minutes IS NULL）跳过该 worker
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

/// `POST /api/v2/admin/worker-pool/auto-allocate` 响应。
#[derive(Debug, Clone, Serialize)]
pub struct AutoAllocateResult {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub mode: AutoAllocateMode,
    pub fill_ratio: f64,
    pub filled: Vec<WorkerFillItem>,
    /// 任一 worker 的 `take_one_from_pool` 返回 `None`（池空）⇒ `pool_empty=true`。
    /// 注意：与单 worker `refill_for_worker` 不同——这里是「所有 worker 中途出现池空」，
    /// 不区分"容量触顶"与"池真空"（与单 worker 同语义；前端按 `pool_empty + filled` 综合判断）。
    pub pool_empty: bool,
}

// ---------------------------------------------------------------------------
// admin assign 端点（follow-up-ux 2026-09-14 新增）
// ---------------------------------------------------------------------------

/// `POST /api/v2/admin/worker-pool/assign`
///
/// 把指定 batch 从候选池（shelf 上的某 shelf）单条分配给 worker。
/// 与 `refill`（循环抢至 max_held）的差异：assign 是**单 batch 拖拽**语义，
/// 不会触顶 max_held；UI 是单条拖拽（前端乐观 UI 与 server 实际一致）。
///
/// `process_id` 可选：若提供则校验 `batch.next_process_id` 必须匹配（防止
/// 工人对未排到该工序的批做 assign）。
#[derive(Debug, Clone, Deserialize)]
pub struct AdminAssignRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    /// 可选；若提供则校验 batch.next_process_id 必须匹配
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_i64_opt")]
    pub process_id: Option<i64>,
}

/// `POST /api/v2/admin/worker-pool/assign` 响应。
///
/// 含新持有的 `TakenItem`（含 part 元数据）+ 分配后 worker 持有数 / 工种上限。
#[derive(Debug, Clone, Serialize)]
pub struct AssignResult {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub taken: TakenItem,
    /// 分配后 worker 的 current_held（含本批次）
    pub current_held: i32,
    /// 分配后 worker 工种的 max_held_batches
    pub max_held: i32,
}