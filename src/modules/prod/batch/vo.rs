//! prod::batch 子模块 VO —— 出参（handler 响应序列化层）
//!
//! 2026-09-29 新增：与 worker_pool / process_chain 同形 VO 模块，
//! 仅 `Serialize` 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//!
//! i64 一律走 `serialize_i64` → JSON string（雪花 ID > 2^53，JS `Number`
//! 会丢精度，参见 `shared::types` 模块 doc）。

use chrono::NaiveDate;
use serde::Serialize;

use crate::shared::types::serialize_i64;

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
    /// 已写入首道 step）。i64 兜底：None → 0，Some(n) → n。
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

/// `POST /api/v2/prod/batches/dispatch` 单条结果（dispatch / bulk-dispatch
/// 列表项通用）。
///
/// `current_process_step_id` 是 `String`（"null" 或 64 位串）以对齐其它域
/// 序列化习惯（雪花 ID 一律 string）。
#[derive(Debug, Clone, Serialize)]
pub struct DispatchResult {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub current_process_step_id: Option<i64>,
    #[serde(serialize_with = "serialize_i64")]
    pub target_process_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub version: i32,
}

/// `POST /api/v2/prod/batches/bulk-dispatch` 顶层响应。
///
/// 设计：partial commit 失败时通过 `failed` 数组携带明细；当前实现是
/// 「任一失败 → 全回滚」，故事务失败时 `succeeded=[]` / `failed=[...]`。
#[derive(Debug, Clone, Serialize)]
pub struct BulkDispatchResult {
    pub succeeded: Vec<DispatchResult>,
    pub failed: Vec<super::dto::DispatchFailureItem>,
}

/// `POST /api/v2/prod/batches/auto-dispatch` 顶层响应。
///
/// 全成功：succeeded.len() == batch_ids.len()，skipped=[]。
/// 任一 batch 因 NO_PROCESS_CHAIN / NO_PROCESS_STEP 被跳过：不影响事务，
/// 落入 skipped 数组；其余 succeeded 仍正常 commit。
/// 任一硬错误（非 skipped）：全回滚，由 caller 重新发起请求。
#[derive(Debug, Clone, Serialize)]
pub struct AutoDispatchResult {
    pub succeeded: Vec<DispatchResult>,
    pub skipped: Vec<super::dto::AutoDispatchSkippedItem>,
}
