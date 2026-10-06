//! prod::queue 的**只读聚合**出参（`/snapshot` 与 `/processes/{process_id}`）
//!
//! 2026-10-08 新增。两个端点取代原 `GET /queue/counts` + `GET /queue/state` +
//! `GET /queue/{process_id}`：原实现让「进程序列板」必须发 N 个请求（每个工序
//! 一次），且 worker 的持有批次要再逐个 worker 拉 —— 两次 N+1。
//!
//! ## 字段取舍：按前端实际消费收敛（逐字段 grep 证据见 `docs/api/queue.md` §5）
//!
//! - 删 `customer_path` —— 前端用 `customer_name` + `parent_customer_name` 自行拼；
//! - 删 `location` 原始 enum —— 候选池项恒为 `PRODUCTION_SHELF`、持有项恒为
//!   `WORKER`，前端用 `shelf_code` 表达位置；
//! - 删 `QueueHeldBatch.shelf_code` —— 持有态 `current_holder_id` 是 worker，
//!   `t_shelf` JOIN 恒不命中，该字段永远是 `null`；
//! - `max_held` 从「独立的 `work_types[]` 数组」改为**直接挂到每个 worker 上**
//!   （前端只按工人渲染，不按工种聚合展示）。
//!
//! i64 主键在**装配处** `.to_string()` 序列化为 JSON string（不 derive 序列化
//! 助手）—— 与 dashboard 域 VO 收口后的做法一致。

use serde::Serialize;

// ============================================================================
// GET /api/v2/prod/queue/snapshot
// ============================================================================

/// `GET /api/v2/prod/queue/snapshot` 顶层出参。
#[derive(Debug, Clone, Serialize)]
pub struct QueueBoardSnapshot {
    /// 有候选批次的工序序列（`pool_count > 0`）。工序元数据在同一次请求里
    /// 批量取回（`id = ANY($1)`），前端不需要为徽标再发一次请求。
    pub processes: Vec<QueueProcessBoard>,
    /// 待下发批次数（`PENDING` / `PROGRAMMING`），供「待下发」tab 标签用。
    /// 口径与 `GET /api/v2/prod/queue/pending` 的 `total` 逐字一致。
    pub pending_count: i64,
    /// RFC3339 带 `+08:00` 偏移（`infra::clock::now_shanghai_iso()`）。
    pub ts: String,
}

/// 序列板上的一道工序。
#[derive(Debug, Clone, Serialize)]
pub struct QueueProcessBoard {
    pub process_id: String,
    pub process_code: String,
    pub process_name: String,
    /// `t_process.color`（`#RRGGBBAA`），未设置时 `null`
    pub color: Option<String>,
    /// `INHOUSE` / `OUTSOURCE`
    pub category: String,
    /// 该工序的候选批次数（跨所有生产货架）
    pub pool_count: i64,
}

// ============================================================================
// GET /api/v2/prod/queue/processes/{process_id}
// ============================================================================

/// `GET /api/v2/prod/queue/processes/{process_id}` 顶层出参。
#[derive(Debug, Clone, Serialize)]
pub struct QueueProcessBoardDetail {
    pub process: QueueProcessMeta,
    pub workers: Vec<QueueWorkerBrief>,
    /// 该工序的候选批次（`items` = `QueuePoolItem`）
    pub items: Vec<QueuePoolItem>,
    /// 候选批次总数（与 `items.len()` 一致，不分页）
    pub total: i64,
    pub ts: String,
}

/// 工序元数据（`t_process` 单行）。
#[derive(Debug, Clone, Serialize)]
pub struct QueueProcessMeta {
    pub process_id: String,
    pub process_code: String,
    pub process_name: String,
    pub color: Option<String>,
}

/// 单个可执行该工序的工人 + 其持有批次。
///
/// `max_held` / `current_held` / `capacity_remaining` 在**装配处**算好
/// （`capacity_remaining = max(0, max_held - current_held)`），SQL 不做这层
/// 推导 —— 三个数都来自不同查询，逐行在 SQL 里算会让「一个 worker 的持有数」
/// 退化成子查询。
#[derive(Debug, Clone, Serialize)]
pub struct QueueWorkerBrief {
    pub worker_id: String,
    pub name: String,
    pub work_type_code: String,
    pub badge_code: String,
    /// 工种上限（`t_work_type.max_held_batches`，未设置时 0）
    pub max_held: i32,
    /// 当前持有批次数
    pub current_held: i32,
    pub capacity_remaining: i32,
    /// 该工人手上压着的批次（`status='IN_PROCESS' AND location='WORKER'`）
    pub held_batches: Vec<QueueHeldBatch>,
}

/// 工人持有的一批。
///
/// **不含 `shelf_code`**（旧 `HeldBatchItem` 有）：持有态
/// `current_holder_id = worker_id`，`t_shelf` JOIN 恒不命中 → 该字段恒 `null`。
#[derive(Debug, Clone, Serialize)]
pub struct QueueHeldBatch {
    pub batch_id: String,
    pub part_id: String,
    pub batch_no: i32,
    pub quantity: i32,
    /// 工单序列号（手工工单可空）
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    pub has_cnc_program: bool,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub applicant_name: Option<String>,
    /// 恒为 `"WORKER"`（写入闸门保证；保留是因为卡片要显示「在工人手上」）
    pub location: String,
    pub note: Option<String>,
    pub version: i32,
}

/// 候选池里的一批（`status='IN_PROCESS' AND location='PRODUCTION_SHELF'`）。
///
/// **不含 `customer_path` 与 `location`**：前者前端自己拼 L1 / L2；后者恒为
/// `"PRODUCTION_SHELF"`，前端用 `shelf_code` 表达位置。
#[derive(Debug, Clone, Serialize)]
pub struct QueuePoolItem {
    pub batch_id: String,
    pub part_id: String,
    pub batch_no: i32,
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub applicant_name: Option<String>,
    /// 当前货架 id（`t_part_batch.current_holder_id`）—— POOL → WORKER 移动的
    /// `from.shelf_id` 数据源，**不能**用用户当前激活货架凑（候选池跨货架）。
    pub shelf_id: String,
    pub shelf_code: String,
    pub shelf_name: String,
    pub is_urgent: bool,
    pub has_cnc_program: bool,
    pub note: Option<String>,
    pub version: i32,
}
