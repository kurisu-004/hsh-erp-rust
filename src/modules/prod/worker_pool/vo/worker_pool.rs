//! worker_pool 域 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDate;
use serde::Serialize;

use crate::modules::prod::worker_pool::model::TakenItem;

/// `GET /api/v2/worker-pool/{process_id}` —— 单条候选批次卡片。
///
/// 字段顺序：`batch_id → part_id → 业务字段 → version`，与既有 `TakenItem` 一致。
/// 多处字段源自 `t_part`（不在 `t_part_batch`）：`name / drawing_no / serial_no /
/// system_delivery_date / is_urgent / note / customer_id / applicant_name`。
#[derive(Debug, Clone, Serialize)]
pub struct PoolBatchItem {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// 工单序列号（手工工单可空）
    pub serial_no: Option<String>,
    /// 工单 / 零件名称
    pub name: String,
    pub drawing_no: String,
    pub system_delivery_date: Option<NaiveDate>,
    /// L2 客户名（叶子）
    pub customer_name: Option<String>,
    /// L1 客户名（一级集团），L2.parent_id 为空时为 None
    pub parent_customer_name: Option<String>,
    /// `"L1 / L2"` 路径；L1 自指仅给 leaf
    pub customer_path: Option<String>,
    /// 申请人字符串列（t_part.applicant_name，非 FK）
    pub applicant_name: Option<String>,
    /// 候选池当前货架 raw enum（如 `"PRODUCTION_SHELF"`）
    pub location: String,
    /// 当前货架 id（t_part_batch.current_holder_id）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub shelf_id: i64,
    pub shelf_code: String,
    pub shelf_name: String,
    /// 是否加急（取自 t_part.is_urgent）
    pub is_urgent: bool,
    /// 工单级备注（t_part.note，DB 无 batch 级 remark 字段；复用）
    pub note: Option<String>,
    /// 2026-09-16 PR-3 批次 step 化：删 `placed_at`
    pub version: i32,
    /// 2026-09-29 新增：是否已上传 G_CODE 数控程序。
    /// 真相源：`EXISTS (SELECT 1 FROM t_part_file WHERE part_id = p.id AND kind = 'G_CODE' AND deleted_at IS NULL)`
    /// 与 `take_one_from_pool` 的优先级排序同源 EXISTS 子查询（已编程 batch 优先 take）。
    pub has_cnc_program: bool,
}

/// 「可执行该工序的工人」单条记录
#[derive(Debug, Clone, Serialize)]
pub struct WorkerBrief {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub worker_id: i64,
    pub name: String,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub work_type_id: i64,
    pub work_type_code: String,
}

/// 「该工序映射到的工种 + max_held」一条记录。
#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeMaxHeld {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub work_type_id: i64,
    pub work_type_code: String,
    pub work_type_name: String,
    pub max_held_batches: Option<i32>,
}

/// `GET /api/v2/worker-pool/{process_id}` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessPoolDetail {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub process_name: String,
    pub workers: Vec<WorkerBrief>,
    pub work_types: Vec<WorkTypeMaxHeld>,
    /// 候选批次总数（与 items.len() 一致，不分页；admin 视角全量）
    pub total: i64,
    pub items: Vec<PoolBatchItem>,
}

/// 单个 worker 的填充结果。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerFillItem {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub worker_id: i64,
    /// 该 worker 的目标（按 mode 计算）
    pub target: i32,
    /// 实际抢到的批次 / 累计分钟数
    pub filled_count: i32,
    /// 是否因业务错跳过该 worker
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

/// `POST /api/v2/admin/worker-pool/auto-allocate` 响应。
#[derive(Debug, Clone, Serialize)]
pub struct AutoAllocateResult {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub process_id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub shelf_id: i64,
    /// mode 字段从 dto::AutoAllocateMode 透传（双向 derive，留在 dto/）
    pub mode: crate::modules::prod::worker_pool::dto::AutoAllocateMode,
    pub fill_ratio: f64,
    pub filled: Vec<WorkerFillItem>,
    /// 任一 worker 的 `take_one_from_pool` 返回 `None`（池空）⇒ `pool_empty=true`
    pub pool_empty: bool,
}

/// `POST /api/v2/prod/pool/move` 响应（2026-09-30 重构）。
///
/// 取代原 `AssignResult`（POOL→WORKER 单边返回）。MoveResult 通用：
/// - 移动方向从 `from_kind` / `to_kind` 透传，前端按需渲染
/// - `new_holder_id` 透传目标位置的 id（POOL=shelf_id，WORKER=worker_id）
/// - `current_held` 仅在 to_kind=WORKER 时有意义（POOL→WORKER 才增减）
/// - `max_held` 同上（仅 WORKER 维度有 max_held 上限）
#[derive(Debug, Clone, Serialize)]
pub struct MoveResult {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    /// from.kind 的字面（`POOL` / `WORKER`）
    pub from_kind: String,
    /// to.kind 的字面（`POOL` / `WORKER`）
    pub to_kind: String,
    /// 移动后 batch 的 `current_holder_id`（shelf_id 或 worker_id）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
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
    /// 仅 POOL→WORKER 移动时填：从 pool 取出的 batch 详情（含 part 元数据）；
    /// 与 `assign_batch_to_worker` 旧 `taken` 字段同源 TakenItem。
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
