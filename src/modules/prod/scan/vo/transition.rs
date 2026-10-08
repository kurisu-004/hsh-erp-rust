//! prod::scan 报工台**写路径**出参 VO
//!
//! 2026-10-10 自 `prod::batch::vo` 搬来：`worker-scan` 端点连同其 DTO / service
//! 一起迁入 `prod::scan`（旧路径 `POST /api/v2/prod/batches/worker-scan` 已下线，
//! **无 alias**，新路径 `POST /api/v2/prod/scan/worker-scan`）。
//!
//! 响应形状逐字不变：仍是 `R<WorkerScanOut>`，`scan` + `refill` 两段。

use serde::Serialize;

use crate::modules::prod::queue::vo::worker::RefillResult;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// worker-scan 核心出参（不含 refill）。
///
/// handler 会把 `scan + refill` 一起装到 [`WorkerScanOut`] 返回；
/// `WorkerScanCoreOut` 是 service 层直接产出的最小投影（与 queue 域的
/// `RefillResult` 解耦，便于 service 层单测）。
///
/// `work_type_id` 与 `badge_code` 是**内部管道字段**：handler 用它把
/// `worker_scan_event` 已经 fetch 过的 worker 信息透传给同事务的
/// `QueueService::refill_for_worker_with_work_type`，避免重复
/// `WorkerRepo::get_by_id` 查询。不暴露到 JSON 响应里。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerScanCoreOut {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// ⚠️ **可能与请求的 `event_type` 不同**：批次在工序链上是最后一道时，
    /// 放回（`RETURNED`）被服务端改判为送检，这里返
    /// `"WORKER_SCAN_INSPECTED"`。消费方（前端文案 + WS 广播）都按**本字段**分支。
    pub event_type: String,
    /// 父装配件 id（仅当送检路径触发父 status 变更时 Some）
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
