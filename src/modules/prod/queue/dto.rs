//! prod::queue DTO（入参 + 校验 + 跨方向 enum）
//!
//! ## DTO/VO 边界
//! 出参结构全在 [`super::vo`]（按写端点 / 下发流 / 聚合板分三个文件）。
//! VO 禁止出现在 axum extractor 反序列化侧。
//!
//! ## 2026-10-08 扩容
//! 自 `prod::batch::dto` 搬入下发流 5 个入参（`ListPendingQuery` /
//! `DispatchRequest` / `DispatchTarget` / `AutoDispatchRequest` /
//! `RecallToPendingRequest`）—— 它们的唯一消费方是队列页的下发 / 召回动作。
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

use crate::shared::types::deserialize_i64;

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

/// `POST /api/v2/prod/queue/move` 入参。
///
/// 字段顺序：`batch_id → version → from → to → note?`。
#[derive(Debug, Clone, Deserialize)]
pub struct MoveRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    /// 批次 OCC 锚，**必填**（无 `#[serde(default)]`）：缺失 → HTTP 422 纯文本
    /// （axum `Json` 提取器），不是业务信封。
    ///
    /// 2026-10-09 新增。此前三个方向都由 service 用「本次事务里刚读到的
    /// `batch.version`」当 `expected_version`，等价于**没有 OCC**：看板数据是
    /// 30s 缓存的快照，期间他人改过批次时，「用户看到 5 件 → 实际移动 3 件」
    /// 会静默成功。服务端读到的 version 现在只用于**兜底对账**（0 行时区分归因），
    /// 不能替代客户端传值。豁免清单见 `CLAUDE.md` §8。
    pub version: i32,
    pub from: MoveLocation,
    pub to: MoveLocation,
    #[serde(default)]
    pub note: Option<String>,
}

// ============================================================================
// 2026-10-08 自 prod::batch::dto 搬入：下发流 / 召回入参
// ============================================================================

/// `GET /api/v2/prod/queue/pending` Query 参数。
///
/// 默认 `limit=200` / `offset=0`（与其它 list 端点惯例一致），允许 caller 覆盖。
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

/// `POST /api/v2/prod/queue/dispatch` —— bulk-only 下发。
///
/// 单批次下发即 `targets.length == 1`；批量多批按 `targets` 数组顺序执行，
/// 任一失败 → 全回滚（事务由 handler 层管）。
///
/// 不带 `shelf_id` / `version`：货架由 service 按 `target_process_id` 在
/// `t_shelf_process` 自动解析，版本号走批次当前 `version` 隐式 OCC。
#[derive(Debug, Clone, Deserialize)]
pub struct DispatchRequest {
    pub targets: Vec<DispatchTarget>,
    /// 可选，落到所有 `t_part_event.note`
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/queue/dispatch` 单条目标。
#[derive(Debug, Clone, Deserialize)]
pub struct DispatchTarget {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    /// **仅无链工单的回落值**（2026-10-09）：工单已绑工序链
    /// （`t_part.process_chain_id IS NOT NULL`）时本字段被**忽略** —— 后端按下发
    /// 链内第一道未软删 step（口径与端点 5 `auto_dispatch_preview` 的
    /// `first_process_id` 逐条相同），并把批次的 `current_process_step_id` 指向
    /// 该 step。工单无链（手工工单的常态）时才按本字段下发。
    ///
    /// 字段**保持必填**（无 `#[serde(default)]`）：漏传仍是 HTTP 422 纯文本，而让
    /// 「有链时忽略」这件事只存在于文档里、把必填改成可选会让「忘了传」从
    /// fail-loud 退化成「按链首下发」这条静默路径。前端可继续照 preview 的
    /// `first_process_id` 填同一个值，两条路径结果一致。
    #[serde(deserialize_with = "deserialize_i64")]
    pub target_process_id: i64,
}

/// `POST /api/v2/prod/queue/auto-dispatch` —— 自动下发预览（只读查询）。
///
/// 返回每个 batch 的「首道工序 + 首货架」+ `skip_reason`，caller 据此构造
/// `dispatch` 的 `targets` 数组。
///
/// `batch_ids` 用 `deserialize_i64_vec_opt` 反序列化：字段缺省 → `None`；
/// 元素按字符串逐个解析（前端发 `"123"` 字符串形态不会触发 422）。
#[derive(Debug, Clone, Deserialize)]
pub struct AutoDispatchRequest {
    #[serde(
        default,
        deserialize_with = "crate::shared::types::deserialize_i64_vec_opt"
    )]
    pub batch_ids: Option<Vec<i64>>,
}

/// `POST /api/v2/prod/queue/recall` 入参。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallToPendingRequest {
    /// 2026-10-08：由 URL path 参数改为 body 字段（对齐本域其余写端点 ID 全走 body 的约定）
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}
