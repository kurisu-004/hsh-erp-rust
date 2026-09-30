//! prod::batch 子模块 DTO —— 入参 + 校验
//!
//! 2026-09-29 新增 + 2026-09-30 重构：
//! - dispatch 统一 bulk-only：单条下发即 `targets.length == 1`
//! - auto-dispatch 改为只读查询（见 `super::vo::AutoDispatchItem`）
//! - bulk-dispatch 端点删除
//!
//! 与 worker_pool / process_chain 等同形 DTO 模块，
//! 仅入参（`Serialize` + 反序列化兜底由 axum `Json` extractor 处理）。
//! 出参结构见 [`super::vo`]。
//!
//! i64 反序列化兜底走 `deserialize_i64` / `deserialize_i64_vec_opt`（与其它域
//! 惯例一致，前端允许 数字 / 字符串 两种形态，雪花 ID 一律 string 避免 JS
//! `Number.MAX_SAFE_INTEGER` 精度截断）。

use serde::Deserialize;

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

/// `POST /api/v2/prod/batches/dispatch` —— bulk-only 下发（2026-09-30 重构）。
///
/// 取代原 `DispatchRequest`（单条）+ `BulkDispatchRequest`（批量）两个 DTO。
/// 单批次下发即 `targets.length == 1`；批量多批按 `targets` 数组顺序执行，
/// 任一失败 → 全回滚（事务由 handler 层管）。
///
/// 不带 shelf_id / version：货架由 service 按 `target_process_id` 在
/// `t_shelf_process` 自动解析（`LIMIT 1`），版本号走 batch 当前 version
/// 隐式 OCC（service 内 fetch batch 后 UPDATE WHERE version = current）。
#[derive(Debug, Clone, Deserialize)]
pub struct DispatchRequest {
    pub targets: Vec<DispatchTarget>,
    /// 可选，落到所有 `t_part_event.note`（2026-09-30 新增，bulk 共享 note）。
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/dispatch` 单条目标。
///
/// 沿用 2026-09-29 原 `BulkDispatchTarget` 字段定义（`batch_id` + `target_process_id`）。
#[derive(Debug, Clone, Deserialize)]
pub struct DispatchTarget {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub target_process_id: i64,
}

/// `POST /api/v2/prod/batches/auto-dispatch` —— 自动下发预览（只读查询）。
///
/// 2026-09-30 重构：原 `auto_dispatch`（写入）改为只读 `auto_dispatch_preview`，
/// 不再真正下发批次，仅返回每个 batch 的「首道工序 + 首货架」+ skip_reason。
/// 实际下发仍走 `POST /api/v2/prod/batches/dispatch`。
///
/// `batch_ids` 用 `deserialize_i64_vec_opt` 反序列化（与本域其它 i64 字段一致）：
/// 字段缺省 → `None`；JSON 数组 → 元素按字符串逐个解析为 `i64`（前端发 `"123"`
/// 字符串形态不会触发 422）。
#[derive(Debug, Clone, Deserialize)]
pub struct AutoDispatchRequest {
    #[serde(default, deserialize_with = "deserialize_i64_vec_opt")]
    pub batch_ids: Option<Vec<i64>>,
}
