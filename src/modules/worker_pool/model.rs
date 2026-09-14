use serde::Serialize;
use crate::shared::types::serialize_i64;

/// `assign` / `refill` 单条返回形状（向后兼容）。
///
/// 既有 `refill_for_worker` 与 `assign_batch_to_worker` 复用此结构。**2026-09-14
/// follow-up-round2 不修改本结构**，避免 break 既有 API 契约。
#[derive(Debug, Clone, Serialize)]
pub struct TakenItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    pub version: i32,
}

/// `WorkerPoolState.held_batches` 单条结构。
///
/// 2026-09-14 follow-up-round2 新增：worker 持有批次的「JOIN t_part +
/// t_customer (L1+L2) + t_applicant + t_shelf」完整 DTO。前端 `WorkerQueueBoard.vue`
/// 直接消费 `held_batches[*].{name / customer_name / applicant_name / shelf_code / location}`
/// 等展示字段，避免按 worker 轮询 K 次单 batch 详情接口的 N+1；同时解决上轮
/// `heldToCard` 字段降级（part_name/customer_name/applicant_name/location/shelf_code
/// 等核心展示字段为空）的问题。
///
/// 与 `TakenItem` 的差异：多 8 个字段（name / customer_name / parent_customer_name /
/// applicant_name / location / shelf_code / note / system_delivery_date 重写）。
/// `drawing_no` 字段不变。
///
/// JOIN 拓扑（见 `part_batch/repo.rs::list_held_by_worker_with_part`）：
/// - `t_part_batch pb`         主表
/// - `t_part p`                INNER JOIN（pb.part_id）
/// - `t_customer c2`           LEFT JOIN（p.customer_id）—— L2 叶子客户
/// - `t_customer c1`           LEFT JOIN（c2.parent_id）—— L1 一级客户
/// - `t_applicant a`           LEFT JOIN（a.name = p.applicant_name，非 FK）
/// - `t_shelf s`               LEFT JOIN（s.id = pb.current_holder_id）——
///   WORKER 持有时 current_holder_id = worker_id 而非 shelf_id，故 shelf_code
///   通常为 None
#[derive(Debug, Clone, Serialize)]
pub struct HeldBatchItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// 工单序列号（手工工单可空）
    pub serial_no: Option<String>,
    /// 图号
    pub drawing_no: String,
    /// 工单 / 零件名称（来自 t_part.name）
    pub name: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    /// L2 客户名（叶子）
    pub customer_name: Option<String>,
    /// L1 客户名（一级集团）
    pub parent_customer_name: Option<String>,
    /// 申请人字符串（来自 t_part.applicant_name 关联到 t_applicant.name，
    /// applicant 软删时回退到 t_part.applicant_name 字符串本身 → 不可空；
    /// 当前实现因 LEFT JOIN 可能为 None）
    pub applicant_name: Option<String>,
    /// 当前 holder 位置 enum（"WORKER"）
    pub location: String,
    /// 当前货架编码（WORKER 持有时 current_holder_id=worker_id 不是 shelf_id，
    /// 故通常为 None；保留字段以便后续承接历史货架追溯）
    pub shelf_code: Option<String>,
    /// 工单级备注（t_part.note）
    pub note: Option<String>,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefillResult {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub taken: Vec<TakenItem>,
    pub pool_empty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessPoolCount {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub pool_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerPoolState {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    pub worker_name: String,
    pub work_type_code: String,
    pub max_held: i32,
    pub current_held: i64,
    pub capacity_remaining: i32,
    pub pool_count_by_process: Vec<ProcessPoolCount>,
    /// 2026-09-14 follow-up-ux 新增 → follow-up-round2 升级为 `Vec<HeldBatchItem>`：
    /// worker 当前持有的完整 batch 列表（JOIN 6 表：t_part_batch + t_part +
    /// t_customer L1+L2 + t_applicant + t_shelf）。避免前端按 worker 轮询 K 次
    /// 单 batch 详情接口的 N+1；UI sink `WorkerQueueBoard.vue` 已对接
    /// `:batches="workerHeld[w.id] ?? []"`。
    pub held_batches: Vec<HeldBatchItem>,
}