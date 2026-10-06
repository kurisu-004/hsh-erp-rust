//! prod::queue 的**下发流**出参（pending / dispatch / auto-dispatch / recall）
//!
//! 2026-10-08 自 `prod::batch::vo` 搬入。搬入理由同 `service/dispatch.rs`：
//! 这 5 类出参只服务下发流一条链，唯一消费方是队列页，与 batch 域的流转 /
//! 返修 / 外协用例无关。
//!
//! 入参见 [`super::dto`]。

use chrono::NaiveDate;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `GET /api/v2/prod/queue/pending` 单条结构（车间 PENDING 批次 + 工单 + 客户
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
    /// `QueueDispatchRepo::update_batch_dispatched`），符合「PENDING 尚未挂 step」语义。
    /// 注：本字段未走 `Option<i64>` 是为了对齐本 VO 整体扁平数字风格（与
    /// `process_chain_id` 同形态）；语义差异由前端按 status 区分。
    #[serde(serialize_with = "serialize_i64")]
    pub current_process_step_id: i64,
    /// `t_part.process_chain_id`（PR-3 step 化后新字段；PENDING 列表透传
    /// 给前端做 UI 关联）。
    #[serde(serialize_with = "serialize_i64")]
    pub process_chain_id: i64,
}

/// `GET /api/v2/prod/queue/pending` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct PendingBatchListOut {
    pub items: Vec<PendingBatchItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ===== dispatch result =====

/// `POST /api/v2/prod/queue/dispatch` 单条结果（bulk-only：单条下发即
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

/// `POST /api/v2/prod/queue/auto-dispatch` 单条预览项。
///
/// 2026-09-30 重构：原 `auto_dispatch` 改为只读 `auto_dispatch_preview`，
/// 不再真正下发批次，仅返回每个 batch 的「首道工序 + 首货架」+ skip_reason。
/// 实际下发仍走 `POST /api/v2/prod/queue/dispatch`，caller 据此构造
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

/// `POST /api/v2/prod/queue/auto-dispatch` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct AutoDispatchResult {
    pub items: Vec<AutoDispatchItem>,
}

/// `POST /api/v2/prod/queue/recall` 出参。
///
/// 2026-10-08 契约变更：原 `POST /prod/batches/{batch_id}/recall-to-pending`
/// 返 `part::vo::PartOut`（工单全量投影），本端点改返本 VO —— 召回的语义锚点是
/// **批次**，返工单投影会让前端为拿 `part_id` 而解析一个上百字段的对象，且批次
/// 自己的 `version`（OCC 锚）根本没在里面，前端下一次操作拿不到正确的版本号。
#[derive(Debug, Clone, Serialize)]
pub struct RecallOut {
    /// 雪花 ID 字符串化
    pub batch_id: String,
    pub part_id: String,
    /// 批次 `version + 1`（写入后）。前端下一次对本批次的操作必须带这个值做 OCC。
    pub version: i32,
}
