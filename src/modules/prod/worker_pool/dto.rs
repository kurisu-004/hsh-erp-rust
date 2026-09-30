//! worker_pool 域 DTO（入参 + 校验 + 跨方向 enum）
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 出参结构（`PoolBatchItem` / `WorkerBrief` / `WorkTypeMaxHeld` /
//! `ProcessPoolDetail` / `WorkerFillItem` / `AutoAllocateResult` / `AssignResult`）
//! 已抽离至 `super::vo`。
//!
//! ## `AutoAllocateMode` 双向 derive 说明
//! `mode` 字段在入参（`AutoAllocateRequest::mode` 反序列化）和出参
//! （`AutoAllocateResult::mode` 序列化）双向使用，故保留双向
//! Deserialize + Serialize derive —— 按 vo-template.md 第 12 条硬约束，
//! 「同一 struct 同时 derive Serialize + Deserialize 被禁，但按业务确实需要的
//! 跨方向 enum 在 doc comment 注明」即视作合理偏离，本域 `AutoAllocateMode`
//! 是这种 enum 模式。

use serde::{Deserialize, Serialize};

use crate::shared::types::{deserialize_i64, deserialize_i64_opt, serialize_i64};

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

/// 自动分配模式：`COUNT` 按批次数填满；`TIME` 按累计预估工时填满。
///
/// 双向 derive —— 见模块头注释。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum AutoAllocateMode {
    Count,
    Time,
}

/// `POST /api/v2/admin/worker-pool/auto-allocate`
#[derive(Debug, Clone, Deserialize)]
pub struct AutoAllocateRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub process_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    pub mode: AutoAllocateMode,
    pub fill_ratio: f64,
}

/// `POST /api/v2/admin/worker-pool/assign`
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

/// `GET /api/v2/prod/worker-pool/counts` —— 单工序候选批次聚合计数条目。
///
/// 2026-09-30 新增：跨所有生产货架聚合 `t_part_batch` 中
/// `status='IN_PROCESS' AND location='PRODUCTION_SHELF' AND deleted_at IS NULL`
/// 的批次数（按 `next_process_id` 维度 GROUP BY）。前端
/// `WorkerQueueBoard.vue` 用 `counts[].count` 给各 tab 标题加 `(N)` 徽标，
/// 不再依赖每 tab 的 worker-pool 详情是否已加载。
///
/// i64 主键走 `serialize_i64` 序列化为字符串（雪花 ID 全链路 string 约定）。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessBatchCount {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub process_name: String,
    /// 该工序候选批次数（cross-shelf 聚合）
    pub count: i64,
}

/// `GET /api/v2/prod/worker-pool/counts` 顶层响应。
///
/// 2026-09-30 新增：admin 视角的全工序候选批次聚合（dashboard 快照型查询）。
/// 仅做 `GROUP BY next_process_id` 单 SQL + service 层二次取 process 元数据，
/// 不分页、不带 WS 广播（与 `GET /state` 同形态的轻量端点）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerPoolCountsOut {
    pub counts: Vec<ProcessBatchCount>,
    /// `counts.iter().map(|c| c.count).sum()`，前端可与 `counts.len()` 区分：
    /// - `total`：候选批次总数（worker 视角有意义）
    /// - `counts.len()`：含候选批次的工序数（dashboard tab 数量）
    pub total: i64,
}
