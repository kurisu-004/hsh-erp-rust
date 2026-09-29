//! prod::batch 子模块 DTO —— 入参 + 校验
//!
//! 2026-09-29 新增：与 worker_pool / process_chain 等同形 DTO 模块，
//! 仅入参（`Serialize` + 反序列化兜底由 axum `Json` extractor 处理）。
//! 出参结构见 [`super::vo`]。
//!
//! i64 反序列化兜底走 `deserialize_i64` / `deserialize_i64_vec_opt`（与其它域
//! 惯例一致，前端允许 数字 / 字符串 两种形态，雪花 ID 一律 string 避免 JS
//! `Number.MAX_SAFE_INTEGER` 精度截断）。

use serde::{Deserialize, Serialize};

use crate::shared::types::{deserialize_i64, deserialize_i64_vec_opt};

/// `GET /api/v2/prod/batches/pending` Query 参数。
///
/// 默认 `limit=200` / `offset=0`（与其它 list 端点惯例一致）。允许 caller
/// 显式覆盖。
#[derive(Debug, Clone, Deserialize)]
pub struct ListPendingQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    200
}

impl Default for ListPendingQuery {
    fn default() -> Self {
        Self {
            limit: default_limit(),
            offset: 0,
        }
    }
}

/// `POST /api/v2/prod/batches/dispatch` —— 单 batch 下发。
///
/// 不带 shelf_id / version：货架由 service 按 `target_process_id` 在
/// `t_shelf_process` 自动解析（`LIMIT 1`），版本号走 batch 当前 version
/// 隐式 OCC（service 内 fetch batch 后 UPDATE WHERE version = current）。
#[derive(Debug, Clone, Deserialize)]
pub struct DispatchRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub target_process_id: i64,
    /// 可选，落到 `t_part_event.note`。
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/bulk-dispatch` —— 批量下发。
///
/// 单事务顺序执行端点 2 逻辑（含货架解析）；任一失败 → 全回滚。
#[derive(Debug, Clone, Deserialize)]
pub struct BulkDispatchRequest {
    pub targets: Vec<BulkDispatchTarget>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BulkDispatchTarget {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub target_process_id: i64,
}

/// `POST /api/v2/prod/batches/auto-dispatch` —— 自动下发（按 part 的 process_chain
/// 首道 step 推导 target_process）。
///
/// 对每个 `batch_ids[i]`：
/// 1. 查 `t_part.process_chain_id`；NULL → skipped (reason='NO_PROCESS_CHAIN')
/// 2. 查 `t_process_chain_step WHERE chain_id ORDER BY sort_order LIMIT 1`；
///    不存在 → skipped (reason='NO_PROCESS_STEP')
/// 3. 否则以 `step.process_id` 作为 `target_process_id` 调 dispatch 核心逻辑。
///
/// 全成功提交；任一硬错误（非 skipped）→ 全回滚。
///
/// `batch_ids` 用 `deserialize_i64_vec_opt` 反序列化（与本域其它 i64 字段一致）：
/// 字段缺省 → `None`；JSON 数组 → 元素按字符串逐个解析为 `i64`（前端发 `"123"`
/// 字符串形态不会触发 422）。
#[derive(Debug, Clone, Deserialize)]
pub struct AutoDispatchRequest {
    #[serde(default, deserialize_with = "deserialize_i64_vec_opt")]
    pub batch_ids: Option<Vec<i64>>,
}

// ===== 出参辅助结构（与 dto 同文件暂存，便于跨子域复用） =====

/// `DispatchResult` 单条失败明细（bulk-dispatch 出参专用）。
#[derive(Debug, Clone, Serialize)]
pub struct DispatchFailureItem {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub code: i32,
    pub message: String,
}

/// `AutoDispatchResult` 单条 skipped 明细。
#[derive(Debug, Clone, Serialize)]
pub struct AutoDispatchSkippedItem {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub reason: String,
}
