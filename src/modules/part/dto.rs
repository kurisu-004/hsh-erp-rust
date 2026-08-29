//! part 域 DTO
//!
//! 对应 Python myERP/schema/part.py。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxOut`：单条详情出参（id 字段用 #[serde(serialize_with = shared::types::serialize_i64)]）
//! - `XxxListItem` / `XxxListOut`：列表分页
//! - `XxxListQuery`：列表查询参数（继承/字段对应 PageQuery）
//!
//! ## Phase F（to-ship 批量通过品检）
//! - `PartOut`：单件详情投影（to-ship / to-inspection / to-process 单/批端点的出参；其它端点复用做最小投影）
//! - `ToShipRequest`：单件入参（`POST /parts/{id}/to-ship`）
//! - `BatchOpItem` / `BatchToShipRequest`：批量入参（`POST /parts/batch-to-ship`）
//! - `BatchOpFailure` / `BatchToXxxOut`：批量出参（含 per-item 失败明细）
//!
//! ## Phase F2（to-inspection 送检 / to-process 指定下一工序）
//! - `ToInspectionRequest`：单件入参（`POST /parts/{id}/to-inspection`）
//! - `BatchToInspectionRequest`：`POST /parts/batch-to-inspection` 批量入参（共享 `BatchToXxxOut` 出参）
//! - `ToProcessRequest`：单件入参（`POST /parts/{id}/to-process`，推荐需求 3）
//! - `ToXxxOut`：单件 / 批量 to-XXX 端点共用的出参 shape（`{ part, new_batch_id }`）
//! - `BatchOpFailure`：单 / 批共享 per-item 失败 DTO（按 `batch_id` 定位失败 item）

use serde::{Deserialize, Serialize};

use crate::modules::worker_pool::dto::WorkerScanEvent;
use crate::modules::worker_pool::model::RefillResult;
use crate::shared::types::{deserialize_i64, serialize_i64, serialize_i64_opt};

/// 工单详情投影（to-ship / to-inspection / to-process 出参；其它端点复用做最小投影）。
///
/// 字段集与 `model::TPartInspected` 完全对齐：仅含 to-XXX 流程与最小
/// `PartOut` 响应必需列。完整业务字段（`applicant_name` / `unit_price` 等）待
/// part 域业务实施时再补全。
#[derive(Debug, Clone, Serialize)]
pub struct PartOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub status: String,
    pub version: i32,
    pub quantity: i32,
    pub order_no: Option<String>,
    pub actual_delivery_date: Option<chrono::NaiveDate>,
    pub updated_at: chrono::NaiveDateTime,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub updated_by: Option<i64>,
}

impl From<crate::modules::part::model::TPartInspected> for PartOut {
    fn from(p: crate::modules::part::model::TPartInspected) -> Self {
        Self {
            id: p.id,
            serial_no: p.serial_no,
            name: p.name,
            drawing_no: p.drawing_no,
            status: p.status,
            version: p.version,
            quantity: p.quantity,
            order_no: p.order_no,
            actual_delivery_date: p.actual_delivery_date,
            updated_at: p.updated_at,
            updated_by: p.updated_by,
        }
    }
}

/// 从完整 `TPart` 投影到 `PartOut`：cancel 流需要 `delivery_note_id` 守卫，
/// 故 read 时用 `PartRepo::get_part_detail` 取完整行（含 delivery_note_id），
/// 直接转 PartOut 响应。
impl From<crate::modules::part::model::TPart> for PartOut {
    fn from(p: crate::modules::part::model::TPart) -> Self {
        Self {
            id: p.id,
            serial_no: p.serial_no,
            name: p.name,
            drawing_no: p.drawing_no,
            status: p.status,
            version: p.version,
            quantity: p.quantity,
            order_no: p.order_no,
            actual_delivery_date: p.actual_delivery_date,
            updated_at: p.updated_at,
            updated_by: p.updated_by,
        }
    }
}

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

/// 单件 / 批量 to-XXX 端点的统一出参 shape。
///
/// `part`：操作后 part 的最新 [`PartOut`] 投影（含 OCC 更新后的 `version`）。
/// `new_batch_id`：仅当 `quantity < target.quantity` 走拆批分支时为
///   `Some(remainder_id)`（拆批后**剩余批次**的 id，留在源状态待后续操作）；
///   整批操作时为 `None`（序列化为 JSON `null`），前端拿到非 null 时应刷新批次列表。
///   用 `serialize_i64_opt` 把 Some 序列化为 JSON 字符串、None 序列化为 `null`，
///   跟 [`PartOut`] 的雪花 id 序列化契约对齐。
/// `synced_assembly_id`：仅当本 part 由 inspection 流触发父装配件 status 翻转时
///   为 `Some(assembly_id)`（handler 据此发 `ASSEMBLY_UPDATED` WS 广播）；
///   无父装配件或父未变更时为 `None`。
#[derive(Debug, Clone, Serialize)]
pub struct ToXxxOut {
    pub part: PartOut,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub new_batch_id: Option<i64>,
    /// 父装配件 id（仅当本 part 由 inspection 流触发父 status 变更时 Some）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub synced_assembly_id: Option<i64>,
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
/// `quantity`：缺省 = 整批（`#[serde(default)]`）；`quantity < target.quantity` →
///   service 拆批；`quantity ≤ 0` → 20111 BIZ_PART_BATCH_INVALID_QUANTITY。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchOpItem {
    pub batch_id: String,
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

/// Per-item 失败明细（item 级别错误，非整批失败）。
///
/// `batch_id`：按 batch 定位失败 item（批量 item 不含 `part_id`，服务从
///   `BatchOpItem::batch_id` 反查后回填；无法 parse 的串落到 `40001` 失败，
///   不进入本结构）。`i64` 而非 `String` 是因为 service 已 parse 过一次，
///   用 `serialize_i64` 序列化为 JSON 字符串与前端 batch_id 字段类型对称。
/// `code` 透传 service 层错误码（20103 / 20104 / 20109 / 20111 / 20511 / 20512 / 40901）；
/// `message` 透传 service 层错误文案（前端可作 toast）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchOpFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub code: i32,
    pub message: String,
}

/// 批量端点统一出参（`batch-to-ship` / `batch-to-inspection` 共用）。
///
/// `submitted`：成功并完成状态流转的 item（含 `PartOut` 最小投影 + 拆批后的
///   `new_batch_id`）；`failed`：item 级别错误（共享 [`BatchOpFailure`]）。
/// `submitted` 与 `failed` 互斥，单 item 不会同时出现在两侧。
#[derive(Debug, Clone, Serialize)]
pub struct BatchToXxxOut {
    pub submitted: Vec<ToXxxOut>,
    pub failed: Vec<BatchOpFailure>,
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

/// worker-scan 核心出参（不含 refill）。
///
/// handler 会把 `scan + refill` 一起装到 [`WorkerScanOut`] 返回；
/// `WorkerScanCoreOut` 是 service 层直接产出的最小投影（与 worker-pool
/// `RefillResult` 解耦，便于 service 层单测）。
///
/// `work_type_id` 与 `badge_code` 是**内部管道字段**：handler 用它把
/// `worker_scan_event` 已经 fetch 过的 worker 信息透传给同事务的
/// `WorkerPoolService::refill_for_worker_with_work_type`，避免重复
/// `WorkerRepo::get_by_id` 查询。不暴露到 JSON 响应里。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerScanCoreOut {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub event_type: String,
    /// 父装配件 id（仅当 INSPECTED 分支触发父 status 变更时 Some）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub synced_assembly_id: Option<i64>,
    /// 内部：透传给 refill，refill 不再 fetch worker。
    #[serde(skip)]
    pub work_type_id: i64,
    /// 内部：refill 写 `TAKEN_FROM_POOL` 事件日志需要 badge_code。
    #[serde(skip)]
    pub badge_code: String,
}

/// worker-scan 端点出参：`scan` + 同事务 refill 结果。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerScanOut {
    pub scan: WorkerScanCoreOut,
    pub refill: RefillResult,
}
