//! prod::batch 子模块 DTO —— 入参 + 校验
//! 2026-09-29 新增 + 2026-09-30 重构：
//! - dispatch 统一 bulk-only：单条下发即 `targets.length == 1`
//! - auto-dispatch 改为只读查询（见 `super::vo::AutoDispatchItem`）
//! - bulk-dispatch 端点删除
//!
//! 与 worker_pool / process_chain 等同形 DTO 模块，
//! 仅入参（`Serialize` + 反序列化兜底由 axum `Json` extractor 处理）。
//! 出参结构见 [`super::vo`]。
//! i64 反序列化兜底走 `deserialize_i64` / `deserialize_i64_opt` /
//! `deserialize_i64_vec_opt`（与其它域惯例一致，前端允许 数字 / 字符串 两种形态，
//! 雪花 ID 一律 string 避免 JS `Number.MAX_SAFE_INTEGER` 精度截断）。
//! 2026-10-02 追加：自 part 域迁入 17 个批次流转入参（见文件末尾小节）。

use serde::Deserialize;

use crate::modules::prod::worker_pool::dto::WorkerScanEvent;
use crate::shared::types::{deserialize_i64, deserialize_i64_opt, deserialize_i64_vec_opt};

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

// 2026-10-02：自 part 域迁入的批次流转 DTO（原 `part/dto.rs` + `part/dto_crud.rs`）
// ============================================================================
//
// 2026-10-02：迁入的根因
//
// 这 17 个入参全部是**以批次为操作对象**的端点（OCC 锚 `t_part_batch.version`），
// 2026-10-02 起 URL 从 `POST /api/v2/parts/{part_id}/…` 硬切到
// `POST /api/v2/prod/batches/{batch_id}/…`，故 DTO 随 handler 一并迁入 prod 域。
// ## 契约变更：子资源 18 条的 `batch_id` 字段**删除**
// `batch_id` 现在是路径参数，再留在请求体里就是二义源。服务端只认 URL 上的那个。
// 错误码语义随之变化（2026-10-02）：批次 id 全局唯一即锚点，不存在「跨 part
// 批次」这一场景，20109 `BIZ_PART_BATCH_NOT_FOUND` 退化为「批次不存在 / 已软删 /
// 状态不是流转起点」；20101 `BIZ_PART_NOT_FOUND` 现在只能经由「批次的 part 已软删」
// 触发，仍可达。

/// `POST /api/v2/prod/batches/{batch_id}/to-ship` 入参。
///
/// 状态机迁移：`INSPECTION` → `READY_TO_SHIP`（含多批次 rollup 守卫 + OCC）。
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 40901。
/// `quantity`：缺省 = 整批；`quantity < batch.quantity` → 部分通过拆批；
/// `quantity ≤ 0` → 20111。
/// `note`：≤ 500 字符；品检备注透传事件日志。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToShipRequest {
    pub version: i32,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/to-inspection` 入参。
///
/// 状态机迁移：`{PENDING, PROGRAMMING, IN_PROCESS}` → `INSPECTION`。
/// `target_inspection_shelf_id`：必填；service 校验 `zone='INSPECTION'` 且
/// `is_active=true`（20511 / 20512）。
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 40901。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToInspectionRequest {
    pub target_inspection_shelf_id: String,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// `POST /api/v2/prod/batches/{batch_id}/to-process` 入参。
///
/// 状态机迁移：`INSPECTION` → `IN_PROCESS`，同时写入目标 production shelf。
/// `shelf_id`：必填；目标生产货架 id（`zone='PRODUCTION'` 且 `is_active=true`）。
/// `next_process_id`：必填；下一道工序 id（与 shelf 映射）。
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 40901。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToProcessRequest {
    pub shelf_id: String,
    pub next_process_id: String,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 批量端点 item 公共结构（`POST /api/v2/prod/batches/to-ship` / `to-inspection`）。
///
/// 无 `part_id`：service 从 `batch_id` 反查 part_id 与 part 当前状态，DTO 更精简。
/// `batch_id`：必填；DB `bigint` 序列化为 JSON 字符串（与 `serialize_i64` 对称）；
/// 缺字段 → 40001 VALIDATION_ERROR；找不到批次 → 20109。
/// `version`：**必填**；不符 → 该 item 落 `failed[] { code: 40901 }`，不中断其余
/// item（per-item savepoint 回滚）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchOpItem {
    pub batch_id: String,
    pub version: i32,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 批量入参（`POST /api/v2/prod/batches/to-inspection`）。
///
/// `target_inspection_shelf_id`：批量共享一个品检架（与单件入参同形校验）。
/// `items.len()` 限制由 service 校验（`BATCH_TO_INSPECTION_MAX_ITEMS`）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchToInspectionRequest {
    pub target_inspection_shelf_id: String,
    pub items: Vec<BatchOpItem>,
}

/// 批量入参（`POST /api/v2/prod/batches/to-ship`）。
///
/// `items.len()` 限制由 service 校验（`BATCH_TO_SHIP_MAX_ITEMS`）。不需要
/// `target_inspection_shelf_id`（to-ship 状态机终态是 `READY_TO_SHIP`，与品检货架无关）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchToShipRequest {
    pub items: Vec<BatchOpItem>,
}

/// `POST /api/v2/prod/batches/worker-scan` 入参。
///
/// 无 Path extractor：`serial_no` 是主键，`batch_id` 仅在多批次歧义时用于消歧。
/// `event_type`：`WorkerScanEvent::RETURNED` / `INSPECTED`。
/// `shelf_id`：必填；RETURNED 时是 worker-scan 货架（PRODUCTION 区），INSPECTED
/// 时是 worker-scan 货架（INSPECTION 区也会校验，按 event_type 分支走）。
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

/// `GET /api/v2/prod/batches/inspection` / `repair` / `repairing` 查询参数（3 条共用）。
///
/// `customer_id` 单值；service 层复用 `expand_customer_id` 展开为 L1+L2 ids
/// （与 `list_parts` 同逻辑）。`keyword` / `serial_no` ILIKE 匹配。
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

/// `POST /api/v2/prod/batches/{batch_id}/deliver` 入参。
///
/// lifecycle 三端点（deliver / complete / start-repair）为 batch 级，OCC 锚定
/// `t_part_batch.version`；状态机守卫读 batch 当前状态。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeliverRequest {
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/complete` 入参。
///
/// 收 `version`（锚 `t_part_batch.version`）。状态机守卫读 batch 当前状态
/// `DELIVERED → COMPLETED`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompleteRequest {
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/start-repair` 入参。
///
/// 守卫：batch 当前 `status='IN_PROCESS'` **且** `is_repairing = false`（REPAIRING
/// 降级为 `t_part_batch.is_repairing` 标记列，本端点**不再发生 status 迁移**，
/// 只把标记置 true）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StartRepairRequest {
    pub version: i32,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/place-on-shelf` 入参。
/// 被 4 个端点复用（place-on-shelf / release-from-programming /
/// receive-from-outsource / receive-from-outsource 的 `PlaceOnShelfRequest` 别名）。
///
/// PENDING → IN_PROCESS（`location='PRODUCTION_SHELF'`）：放到指定生产货架。
/// service 层校验 `shelf ↔ process` 映射（`BIZ_SHELF_PROCESS_NOT_MAPPED` 422）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlaceOnShelfRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub next_process_id: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/recall-to-pending` 入参。
///
/// ON_SHELF（IN_PROCESS+PRODUCTION_SHELF）或 PROGRAMMING → PENDING：
/// 召回未领批次回 PENDING。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallToPendingRequest {
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/send-to-outsource` 入参。
///
/// PENDING / IN_PROCESS+PRODUCTION_SHELF → OUTOURCE（`location='OUTSOURCE_COMPANY'`）。
/// `outsource_company_id` + `process_id` 必填；`quote_id` 必填（APPROVED 报价，
/// service 在同事务内 INSERT `t_outsource_shipment`）；`direct=true` 跳过 quote
/// 校验（DIRECT 免审批占位；Phase 2 stub：返回 501 NOT_IMPLEMENTED）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendToOutsourceRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub outsource_company_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub process_id: i64,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub quote_id: Option<i64>,
    #[serde(default)]
    pub direct: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection` 入参。
///
/// OUTSOURCE → INSPECTION（直接送检）。`shelf_id` 必填，service 层校验 zone=INSPECTION。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReceiveFromOutsourceToInspectionRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(default)]
    pub auto_pass_inspection: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/complete-repair` 入参。
///
/// 要求 batch `is_repairing = true`（确实在返修中）。去向由 shelf.zone 决定：
/// PRODUCTION → `IN_PROCESS`（落回生产架、重新入池，写 `next_process_id` +
/// step）或 INSPECTION → `INSPECTION`（送检区、出池）。两条路径都清
/// `is_repairing`。shelf.zone=PRODUCTION 时 next_process_id 必填且需校验
/// shelf↔process 映射。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompleteRepairRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub next_process_id: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/repair-dispatch` 入参。
///
/// 一步式返修下发（`start_repair + complete_repair` 合并）：从 IN_PROCESS / INSPECTION
/// / READY_TO_SHIP 入口直达目标状态。`shelf_id` 必填（PRODUCTION 或 INSPECTION 区）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RepairDispatchRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub next_process_id: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/split` 入参。
///
/// 拆出部分量为新批次（继承源批次 status/location/holder/next_process；
/// 不继承 delivery_note_id）。`quantity` ∈ [1, source_batch.quantity - 1]。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SplitBatchRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub quantity: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/cancel` 入参。
///
/// 批次级取消：终态保护，非终态 → CANCELLED。`version` OCC 守。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CancelBatchRequest {
    pub version: i32,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/pick-up` 入参（手动 pick-up 兜底）。
///
/// PENDING / IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER。
/// `worker_id` 必填（持有件工人）；`shelf_id` 必填（当前批次所在货架；service 层
/// 仅校验存在 + 同 shelf ↔ process 映射）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PickUpRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/scan-inspect` 入参。
///
/// 扫码快捷品检（一步式：`{PENDING, PROGRAMMING, IN_PROCESS}` → INSPECTION →
/// READY_TO_SHIP 或「返修中」，由 `pass` 字段决定）。
/// `target_inspection_shelf_id` 必填（INSPECTION 区 active）。
/// `pass=false`：`status='IN_PROCESS'` + `is_repairing=true`（批次停在送检架，
/// `shelf_id` + `next_process_id` 供随后的 `complete-repair` 落回生产架用）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScanInspectRequest {
    pub pass: bool,
    #[serde(deserialize_with = "deserialize_i64")]
    pub target_inspection_shelf_id: i64,
    pub version: i32,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub shelf_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub next_process_id: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/scan/deliver` 入参（无 path；从 `serial_no` 反查）。
///
/// 司机扫码发货：`part_serial_no` + `worker_badge_code`。Service 层校验
/// `worker.work_type.code == '送货司机'`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScanDeliverPartRequest {
    pub part_serial_no: String,
    pub worker_badge_code: String,
    #[serde(default)]
    pub note: Option<String>,
}
