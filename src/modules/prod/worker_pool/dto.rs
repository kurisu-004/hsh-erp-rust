//! worker_pool 域 DTO（入参 + 校验 + 跨方向 enum）
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 出参结构（`PoolBatchItem` / `WorkerBrief` / `WorkTypeMaxHeld` /
//! `ProcessPoolDetail` / `WorkerFillItem` / `AutoAllocateResult` / `MoveResult`）
//! 已抽离至 `super::vo`。
//!
//! ## `AutoAllocateMode` 双向 derive 说明
//! `mode` 字段在入参（`AutoAllocateRequest::mode` 反序列化）和出参
//! （`AutoAllocateResult::mode` 序列化）双向使用，故保留双向
//! Deserialize + Serialize derive —— 按 vo-template.md 第 12 条硬约束，
//! 「同一 struct 同时 derive Serialize + Deserialize 被禁，但按业务确实需要的
//! 跨方向 enum 在 doc comment 注明」即视作合理偏离，本域 `AutoAllocateMode`
//! 是这种 enum 模式。
//!
//! ## `MoveLocation` tagged enum（2026-09-30 重构）
//! 原 `admin_remove` / `admin_assign` 合并为通用 `POST /api/v2/prod/pool/move`，
//! 端点接受 `from` / `to` 两个 tagged enum 标识 batch 当前位置与目标位置。
//! 序列化形态：
//! - `{"kind":"POOL",  "shelf_id":100}`
//! - `{"kind":"WORKER","worker_id":50}`
//!
//! 不支持 `POOL → POOL`（视为非法，service 返回 `40001 VALIDATION_ERROR`）。

use serde::{Deserialize, Serialize};

use crate::shared::types::{deserialize_i64, serialize_i64};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WorkerScanEvent {
    RETURNED,
    INSPECTED,
}

/// POST /api/v2/prod/pool/refill
#[derive(Debug, Clone, Deserialize)]
pub struct AdminRefillRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
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

/// `POST /api/v2/prod/pool/auto-allocate`
#[derive(Debug, Clone, Deserialize)]
pub struct AutoAllocateRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub process_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    pub mode: AutoAllocateMode,
    pub fill_ratio: f64,
}

/// `POST /api/v2/prod/pool/move` —— 通用移动端点。
///
/// 把 batch 在 `from` → `to` 之间移动，覆盖 pool ↔ worker（worker ↔ worker 也支持）。
/// 该 enum 取代原 `AdminAssignRequest`（POOL→WORKER 单边）与 `AdminRemoveRequest`
/// （WORKER→POOL 单边）。`POOL → POOL` / `WORKER → WORKER` 同 kind 视为非法
/// （service 抛 `40001 VALIDATION_ERROR`）。
///
/// 序列化形态：
/// ```jsonc
/// {"kind":"POOL",   "shelf_id": 100}
/// {"kind":"WORKER", "worker_id": 50}
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "UPPERCASE")]
pub enum MoveLocation {
    /// batch 当前在生产货架（候选池）。`shelf_id` 必须与 batch.current_holder_id 一致。
    Pool {
        #[serde(deserialize_with = "deserialize_i64")]
        shelf_id: i64,
    },
    /// batch 当前被 worker 持有。`worker_id` 必须与 batch.current_holder_id 一致。
    Worker {
        #[serde(deserialize_with = "deserialize_i64")]
        worker_id: i64,
    },
}

/// `POST /api/v2/prod/pool/move` 入参。
///
/// 字段顺序与 plan §2.1 一致：`batch_id → from → to → note?`。
#[derive(Debug, Clone, Deserialize)]
pub struct MoveRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub from: MoveLocation,
    pub to: MoveLocation,
    #[serde(default)]
    pub note: Option<String>,
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
