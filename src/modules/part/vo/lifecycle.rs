//! part 域 lifecycle state 出参 VO（2026-09-22 PR4 重构）

use serde::Serialize;

use crate::modules::prod::worker_pool::model::RefillResult;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

use super::part::PartOut;

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

/// `GET /parts/location-tree` 出参：按 shelf/status 聚合的位置树。
#[derive(Debug, Clone, Serialize)]
pub struct LocationTreeNodeOut {
    pub id: String,
    pub label: String,
    pub kind: String, // "OFFICE" / "PRODUCTION_SHELF" / "WORKER" / "INSPECTION_SHELF" / "OUTSOURCE_COMPANY"
    #[serde(serialize_with = "serialize_i64_opt")]
    pub parent_id: Option<i64>,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocationTreeOut {
    pub items: Vec<LocationTreeNodeOut>,
}