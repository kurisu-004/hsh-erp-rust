use serde::{Deserialize, Serialize};
use crate::shared::types::{deserialize_i64, serialize_i64};

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
    /// 批次上架时间（t_part_batch.placed_at）—— 用于前端展示「积压多久」
    pub placed_at: chrono::NaiveDateTime,
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