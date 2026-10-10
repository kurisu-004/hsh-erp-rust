//! prod::queue 的**写端点**出参（refill / move / auto-allocate）
//!
//! 2026-10-08 从原 `vo/worker_pool.rs` + `model.rs` 合并而成。
//!
//! ## 相对上一版删掉的 struct 与原因
//!
//! | 被删 | 原因 |
//! |---|---|
//! | `PoolBatchItem` | 只被 `GET /queue/{process_id}` 消费，该端点被 `GET /queue/processes/{process_id}` 取代；字段集见 [`super::board::QueuePoolItem`] |
//! | `WorkerBrief` | 同上，字段集见 [`super::board::QueueWorkerBrief`] |
//! | `WorkTypeMaxHeld` | 前端零消费（`max_held` 改由 board 端点直接挂到 worker 上，见 [`super::board::QueueWorkerBrief`]） |
//! | `ProcessPoolDetail` | 被 [`super::board::QueueProcessBoard`] 取代 |
//! | `HeldBatchItem` | 被 [`super::board::QueueHeldBatch`] 取代（旧版含恒为 `null` 的 `shelf_code`） |
//! | `ProcessPoolCount` / `WorkerPoolState` | 只服务已删除的 `GET /queue/state` |
//!
//! 逐字段的「前端零消费」grep 证据见 `docs/api/queue.md` §5。

use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `take_one_from_pool` / `take_specific_from_pool` 单条返回形状。
///
/// 被 `RefillResult.taken` 与 `MoveResult.taken` 共用。`has_cnc_program` 透传
/// repo 层 EXISTS 子查询结果（`t_part_file.kind='G_CODE'`），与候选池 / 持有列表
/// 同一口径，保证「已编程批次优先 take」的排序依据在三处一致。
#[derive(Debug, Clone, Serialize)]
pub struct TakenItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    pub version: i32,
    pub has_cnc_program: bool,
}

/// `POST /api/v2/prod/queue/refill` 出参。
#[derive(Debug, Clone, Serialize)]
pub struct RefillResult {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    /// 2026-10-10：改 `Option<i64>`，`null` = 跨全部货架取料（worker-scan 路径）。
    /// 省略本就没有信息量（refill 不移动批次，只是把池里的批次派给工人），但保留
    /// 它是为了让管理员限架场景（「在某架上抢料」）能被前端区分显示。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub shelf_id: Option<i64>,
    pub taken: Vec<TakenItem>,
    /// **一批也没抢到**。⚠️ **语义一个字都不许改** —— 它是既有 wire 契约、前端
    /// 已在读（`handler/pool.rs` 的 WS 广播条件历史上就是直接读它）。
    ///
    /// ⚠️ 它**无法区分**「池里没有可抢的」与「工人已经拿满了」两种截然不同的
    /// 情况：`take_one_from_pool` 对两者**都**返回 `None`（前者是 SELECT 无行，
    /// 后者是 `held < max` 守卫不通过）。2026-10-14 之前两者在响应里长得一模一样，
    /// 运维只能对着代码猜。区分靠新增的 [`Self::stopped_reason`]，**不改本字段**。
    pub pool_empty: bool,
    /// 2026-10-14 新增：`taken` 为空时的**具体**停止原因，取值二选一：
    /// - `"POOL_EMPTY"` —— 候选池里没有该工种可加工、且在架上的批次；
    /// - `"WORKER_FULL"` —— 工人已持有 `max_held_batches` 批，容量守卫拦死。
    ///
    /// 抢到任何一批时为 `None`，且因 `skip_serializing_if` 而**整个 key 不出现**
    /// （与本 struct 内 `MoveResult.shelf_id` 的既有写法同款）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_reason: Option<String>,
}

/// [`RefillResult::stopped_reason`] / [`WorkerFillItem::skipped_reason`] 的取值常量。
///
/// ⚠️ **改一同步二义务**：`RefillResult.stopped_reason` 与
/// `AutoAllocateResult` 里填 `skipped_reason` 的地方共用这两个字面量
/// （见 `QueueService::refill_for_worker_with_work_type` 与
/// `auto_allocate_for_process`）。前端按字面量做分支，不要随手改。
pub mod stopped_reason {
    /// 候选池里没有可抢的批次。
    pub const POOL_EMPTY: &str = "POOL_EMPTY";
    /// 工人容量已触顶（`held >= max_held_batches`），或工种未配上限（按 cap 0 拦死）。
    pub const WORKER_FULL: &str = "WORKER_FULL";
}

/// 单个 worker 的自动分配填充结果。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerFillItem {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    /// 该 worker 的目标（按 mode 计算）
    pub target: i32,
    /// 实际抢到的批次 / 累计分钟数
    pub filled_count: i32,
    /// 因业务错跳过该 worker 时填原因；
    /// 2026-10-14 起 ALSO 覆盖「没抢够」：本 worker 因池空 / 已拿满而提前 `break`
    /// 时填 [`stopped_reason::POOL_EMPTY`] / [`stopped_reason::WORKER_FULL`]
    /// （此前恒 `None`，见 `QueueService::auto_allocate_for_process`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

/// `POST /api/v2/prod/queue/auto-allocate` 出参。
#[derive(Debug, Clone, Serialize)]
pub struct AutoAllocateResult {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    /// mode 从 `dto::AutoAllocateMode` 透传（该 enum 双向 derive，留在 `dto/`）
    pub mode: crate::modules::prod::queue::dto::AutoAllocateMode,
    pub fill_ratio: f64,
    pub filled: Vec<WorkerFillItem>,
    /// **任一** worker 的 `take_one_from_pool` 返回 `None`（早退）⇒ `true`。
    ///
    /// ⚠️ **2026-10-14 订正**：本字段此前的 doc 写「（池空）」，但
    /// `take_one_from_pool` 返回 `None` 有**两种**成因 —— SELECT 无行（池空）
    /// 与 `held < max` 守卫不通过（工人已拿满）。两种情况都置 `true`。
    /// **判据的修正在 service 层逐 worker 填 [`WorkerFillItem::skipped_reason`]**
    /// （`POOL_EMPTY` / `WORKER_FULL`）；本字段只回答「有没有人没抢够」，
    /// 不回答「为什么」—— 它与 [`RefillResult::pool_empty`] 是同一形状的既有契约，
    /// 不改。
    pub pool_empty: bool,
}

/// `POST /api/v2/prod/queue/move` 出参。
///
/// 通用移动端点覆盖 POOL ↔ WORKER + WORKER ↔ WORKER 三方向，前端按
/// `from_kind` / `to_kind` 推断方向：
/// - `new_holder_id` = 目标位置的 id（POOL=shelf_id，WORKER=worker_id）
/// - `current_held` / `max_held` 仅 `to_kind=WORKER` 时有意义
#[derive(Debug, Clone, Serialize)]
pub struct MoveResult {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// `from.kind` 的字面（`POOL` / `WORKER`）
    pub from_kind: String,
    /// `to.kind` 的字面（`POOL` / `WORKER`）
    pub to_kind: String,
    /// 移动后 batch 的 `current_holder_id`（shelf_id 或 worker_id）
    #[serde(serialize_with = "serialize_i64")]
    pub new_holder_id: i64,
    /// 移动后 batch 的 location（`PRODUCTION_SHELF` 或 `WORKER`）
    pub new_location: String,
    pub version: i32,
    /// 仅 to_kind=WORKER 时填：移动后 worker 的 current_held（含本批次）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_held: Option<i32>,
    /// 仅 to_kind=WORKER 时填：worker 工种的 max_held_batches
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_held: Option<i32>,
    /// 涉及的候选池货架 id（雪花 ID 序列化成 JSON 字符串）：
    /// - POOL→WORKER：填 `from.shelf_id`（批次离开的货架）
    /// - WORKER→POOL：填 `to.shelf_id`（批次落回的货架）
    /// - WORKER→WORKER：不填
    ///
    /// 2026-10-03 修复：此前漏标序列化器，序列化成 JSON number，前端 Zod 守门
    /// 抛 `expected string, received number`。取 `Option<i64>` +
    /// `serialize_i64_opt` 而非 `Option<String>`：与本 struct 其余雪花字段
    /// （`batch_id` / `new_holder_id`）及内嵌 `TakenItem` 风格一致，service 侧
    /// 只需一个 serde 属性、不必为单一字段改类型。
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub shelf_id: Option<i64>,
    /// 仅 POOL→WORKER 移动时填：从 pool 取出的 batch 详情。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken: Option<TakenItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// POOL→WORKER：service 必填 `shelf_id`（值 = `from.shelf_id`），
    /// 故「方向为 POOL→WORKER 且 `shelf_id` 为 None」不是现实可出现的组合。
    fn move_result_pool_to_worker(shelf_id: Option<i64>) -> MoveResult {
        MoveResult {
            batch_id: 1590000000000000002,
            from_kind: "POOL".to_string(),
            to_kind: "WORKER".to_string(),
            new_holder_id: 1590000000000000003,
            new_location: "WORKER".to_string(),
            version: 2,
            current_held: Some(1),
            max_held: Some(5),
            shelf_id,
            taken: None,
        }
    }

    /// WORKER→WORKER：唯一 `shelf_id` 现实为 `None` 的方向（`to_kind=WORKER`
    /// 故 `current_held` / `max_held` 照填）。
    fn move_result_worker_to_worker() -> MoveResult {
        MoveResult {
            batch_id: 1590000000000000002,
            from_kind: "WORKER".to_string(),
            to_kind: "WORKER".to_string(),
            new_holder_id: 1590000000000000003,
            new_location: "WORKER".to_string(),
            version: 2,
            current_held: Some(1),
            max_held: Some(5),
            shelf_id: None,
            taken: None,
        }
    }

    /// 雪花 ID 走 `serialize_i64_opt` ⇒ JSON string，且十进制内容与传入值一致。
    #[test]
    fn move_result_shelf_id_serializes_as_string() {
        let value =
            serde_json::to_value(move_result_pool_to_worker(Some(1590000000000000001))).unwrap();
        assert_eq!(
            value["shelf_id"],
            serde_json::Value::String("1590000000000000001".into())
        );
    }

    /// `skip_serializing_if` 优先于 `serialize_i64_opt` ⇒ None 时 key 不出现。
    #[test]
    fn move_result_omits_shelf_id_when_none() {
        let value = serde_json::to_value(move_result_worker_to_worker()).unwrap();
        // 先断言是 object：`Value::get` 对非 object 同样返回 None，
        // 直接 get 会让「序列化结果不是 object」这条异常路径静默通过。
        let object = value
            .as_object()
            .expect("MoveResult 应序列化为 JSON object");
        assert!(object.get("shelf_id").is_none());
    }
}
