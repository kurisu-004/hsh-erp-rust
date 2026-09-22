//! part 域 DTO
//!
//! 对应 Python myERP/schema/part.py。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxListQuery`：列表查询参数（继承/字段对应 PageQuery）
//!
//! 出参（*Out 类型）已迁移到 `super::vo`（2026-09-22 PR4 重构）：DTO 仅含
//! axum extractor 反序列化目标（`#[derive(Deserialize)]`），VO 仅含 handler
//! 返回序列化目标（`#[derive(Serialize)]`），二者不再同文件。
//!
//! ## Phase F（to-ship 批量通过品检）
//! - `ToShipRequest`：单件入参（`POST /parts/{id}/to-ship`）
//! - `BatchOpItem` / `BatchToShipRequest`：批量入参（`POST /parts/batch-to-ship`）
//!
//! ## Phase F2（to-inspection 送检 / to-process 指定下一工序）
//! - `ToInspectionRequest`：单件入参（`POST /parts/{id}/to-inspection`）
//! - `BatchToInspectionRequest`：`POST /parts/batch-to-inspection` 批量入参
//! - `ToProcessRequest`：单件入参（`POST /parts/{id}/to-process`，推荐需求 3）

use serde::Deserialize;

use crate::modules::prod::worker_pool::dto::WorkerScanEvent;
use crate::shared::types::{deserialize_i64, deserialize_i64_opt};

/// 单件 to-ship 入参（`POST /parts/{id}/to-ship`）。
///
/// 状态机迁移：`INSPECTION` → `READY_TO_SHIP`（含多批次 rollup 守卫 + OCC）。
/// `batch_id`：**必填**（2026-08-29 起）；caller 侧乐观锁需要明确锚定批次，
///   不再支持「按状态唯一匹配」推断。找不到 / 不属于该 part → 20109。
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 40901。
/// `quantity`：缺省 = 整批；`quantity ≤ 0` → 20111。
/// `note`：≤ 500 字符；品检备注透传事件日志。
#[derive(Debug, Clone, Deserialize)]
pub struct ToShipRequest {
    pub batch_id: String,
    pub version: i32,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub note: Option<String>,
}

/// 单件 to-inspection 入参（`POST /parts/{id}/to-inspection`）。
///
/// 状态机迁移：`{PENDING, PROGRAMMING, IN_PROCESS}` → `INSPECTION`。
/// `target_inspection_shelf_id`：必填；service 校验 `zone='INSPECTION'` 且
///   `is_active=true`（20511 / 20512）。
/// `batch_id`：**必填**（2026-08-29 起）；caller 侧乐观锁需要明确锚定批次，
///   不再支持「按状态唯一匹配」推断。找不到 / 不属于该 part → 20109。
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 40901。
/// `quantity`：缺省 = 整批；`quantity ≤ 0` → 20111。
/// `note`：≤ 500 字符；品检备注透传事件日志。
#[derive(Debug, Clone, Deserialize)]
pub struct ToInspectionRequest {
    pub target_inspection_shelf_id: String,
    pub batch_id: String,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 单件 to-process 入参（`POST /parts/{id}/to-process`，推荐需求 3）。
///
/// 状态机迁移：`INSPECTION` → `IN_PROCESS`，同时写入目标 production shelf。
/// `shelf_id`：必填；目标生产货架 id（`zone='PRODUCTION'` 且 `is_active=true`）。
/// `next_process_id`：必填；下一道工序 id（与 shelf 映射）。
/// `batch_id`：**必填**（2026-08-29 起）；caller 侧乐观锁需要明确锚定批次，
///   不再支持「按状态唯一匹配」推断。找不到 / 不属于该 part → 20109。
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 40901。
/// `quantity`：缺省 = 整批；`quantity ≤ 0` → 20111。
/// `note`：≤ 500 字符；品检备注透传事件日志。
#[derive(Debug, Clone, Deserialize)]
pub struct ToProcessRequest {
    pub shelf_id: String,
    pub next_process_id: String,
    pub batch_id: String,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 批量端点 item 公共结构（`POST /parts/batch-to-ship` / `batch-to-inspection`）。
///
/// 无 `part_id`：service 从 `batch_id` 反查 part_id 与 part 当前状态，DTO 更精简，
/// 单件 / 批量端点共享同一 item shape。
///
/// `batch_id`：必填；DB `bigint` 序列化为 JSON 字符串（与 `serialize_i64` 对称）；
///   缺字段 → 40001 VALIDATION_ERROR；找不到批次 → 20109 BIZ_PART_BATCH_NOT_FOUND。
///   注意 `batch_id` 不是 `Option<String>`——service 把它作为反查 part 的唯一键，
///   必须存在。
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 该 item 落
///   `failed[] { code: 40901 }`，不中断其余 item（per-item savepoint 回滚）。
/// `quantity`：缺省 = 整批（`#[serde(default)]`）；`quantity < target.quantity` →
///   service 拆批；`quantity ≤ 0` → 20111 BIZ_PART_BATCH_INVALID_QUANTITY。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchOpItem {
    pub batch_id: String,
    pub version: i32,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 批量入参（`POST /parts/batch-to-inspection`）。
///
/// `target_inspection_shelf_id`：批量共享一个品检架（与单件入参同形校验）。
/// `items.len()` 限制由 service 校验（`BATCH_TO_INSPECTION_MAX_ITEMS`，见 service 层）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchToInspectionRequest {
    pub target_inspection_shelf_id: String,
    pub items: Vec<BatchOpItem>,
}

/// 批量入参（`POST /parts/batch-to-ship`）。
///
/// `items.len()` 限制由 service 校验（`BATCH_TO_SHIP_MAX_ITEMS`，见 service 层）。
/// 不需要 `target_inspection_shelf_id`（to-ship 状态机终态是
/// `READY_TO_SHIP`，与品检货架无关）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchToShipRequest {
    pub items: Vec<BatchOpItem>,
}

/// worker-scan 入参（`POST /parts/worker-scan`，Task 8）。
///
/// `serial_no` / `badge_code`：扫码原始字符串（service 层反查）。
/// `event_type`：`WorkerScanEvent::RETURNED` / `INSPECTED`。
/// `shelf_id`：必填；RETURNED 时是 worker-scan 货架（PRODUCTION 区），INSPECTED
///   时是 worker-scan 货架（INSPECTION 区也会校验，按 event_type 分支走）。
/// `next_process_id`：仅 RETURNED 必填；缺 / 非法 → 40001。
/// `target_inspection_shelf_id`：仅 INSPECTED 必填；缺 / 非法 → 40001。
/// `batch_id`：可选；多批次歧义时 caller 显式指定以消除歧义。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerScanRequest {
    pub serial_no: String,
    pub badge_code: String,
    pub event_type: WorkerScanEvent,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(default)]
    pub next_process_id: Option<String>,
    #[serde(default)]
    pub target_inspection_shelf_id: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
}

// ===== Inspection Batch List =====

/// `GET /parts/inspection-batches` 查询参数。
///
/// 与 Python `list_inspection_batches(*, keyword, customer_id, serial_no,
/// planned_delivery_date_from, planned_delivery_date_to, limit=200, offset=0)`
/// 对齐。`customer_id` 单值；service 层复用 `expand_customer_id` 展开为
/// L1+L2 ids（与 `list_parts` 同逻辑）。`keyword` / `serial_no` ILIKE 匹配。
/// `planned_delivery_date_*` 作用于 `t_part.planned_delivery_date`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InspectionBatchListQuery {
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub serial_no: Option<String>,
    #[serde(default)]
    pub planned_delivery_date_from: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub planned_delivery_date_to: Option<chrono::NaiveDate>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub offset: Option<i64>,
}