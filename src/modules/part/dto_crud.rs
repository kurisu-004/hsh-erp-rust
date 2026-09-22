//! part 域 CRUD + lifecycle DTO（Phase PR-CRUD 2026-08-25）
//!
//! 命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxListQuery`：列表查询参数
//!
//! 出参（*Out 类型）已迁移到 `super::vo`（2026-09-22 PR4 重构）：DTO 仅含
//! axum extractor 反序列化目标（`#[derive(Deserialize)]`），VO 仅含 handler
//! 返回序列化目标（`#[derive(Serialize)]`），二者不再同文件。

use serde::Deserialize;

use crate::shared::types::{deserialize_i64, deserialize_i64_opt};

// ===== Create =====

/// `POST /parts` 入参：单件创建工单。
#[derive(Debug, Clone, Deserialize)]
pub struct PartCreateRequest {
    pub name: String,
    pub drawing_no: String,
    pub applicant_name: String,
    pub quantity: i32,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    #[serde(default)]
    pub is_urgent: bool,
    #[serde(deserialize_with = "deserialize_i64")]
    pub customer_id: i64,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub assembly_id: Option<i64>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
}

// ===== Batch create =====

/// `POST /parts/batch` 单 item 入参：与 `PartCreateRequest` 字段集对齐，
/// 但 `customer_id` 提到 batch 级别（共享）。
///
/// 2026-09-16 M2-B 新增可选字段：
/// - `drawing_file`：上传图纸 PDF（kind=DRAWING）绑定
/// - `model3d_file`：上传 3D 模型（kind=3D_MODEL）绑定
///
/// 两个字段都形如 [`FileBindingIn`]，由前端从 `POST /part-files/upload-intents`
/// 拿到 `tmp_key` 后填回。`batch_create_parts` service 会先并发 head/copy 所有
/// binding 项，**任一失败** → 整体回滚（让用户重试整批）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartBatchCreateItem {
    pub name: String,
    pub drawing_no: String,
    pub applicant_name: String,
    pub quantity: i32,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    #[serde(default)]
    pub is_urgent: bool,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub assembly_id: Option<i64>,
    /// 2026-09-16 M2-B 新增：上传图纸 PDF 绑定（kind=DRAWING，file_type=PDF）
    #[serde(default)]
    pub drawing_file: Option<FileBindingIn>,
    /// 2026-09-16 M2-B 新增：上传 3D 模型绑定（kind=3D_MODEL，file_type 由扩展名推导）
    #[serde(default)]
    pub model3d_file: Option<FileBindingIn>,
}

/// `POST /parts/batch` 单个文件绑定子结构（drawing_file / model3d_file）。
///
/// 由前端在拿到 `POST /part-files/upload-intents` 返回的 `tmp_key` 后填回。
/// `content_sha256` 必须与 upload-intents 提交时一致（CAS 命中场景下
/// upload-intents 返回 `dedup_hit=true`，前端跳过上传，把 `existing_file` 拼
/// 回 PartFileOut，本字段为 None——即 `binding` 也为 None）。
///
/// 2026-09-16 M2-B 新增。
#[derive(Debug, Clone, Deserialize)]
pub struct FileBindingIn {
    pub tmp_key: String,
    pub content_sha256: String,
    pub original_filename: String,
    #[serde(deserialize_with = "deserialize_i64")]
    pub file_size: i64,
    pub content_type: String,
}

/// `POST /parts/batch` 入参：批量创建（共享 customer_id）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartBatchCreateRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub customer_id: i64,
    pub items: Vec<PartBatchCreateItem>,
}

// ===== Update =====

/// `PUT /parts/{id}` 入参：字段可选 UPDATE。
///
/// `version` 必填（OCC）；其它字段未传 → DB 不动。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `actual_delivery_date` 入参
/// （t_part 列已删；实际交付日期由 t_part_event DELIVERED 事件派生，不接
/// 受手工改）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartUpdateRequest {
    pub version: i32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub applicant_name: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
}

// ===== List =====

/// `GET /parts` 查询参数。
///
/// `customer_id`：单值；service 层用 `expand_customer_id` 展开为 L1+L2 ids。
/// `status` / `statuses`：单值 / 多值互不冲突；service 层二选一传入。
/// `locations` / `holder_ids`：逗号分隔。`locations` 是 `t_part_batch.location`
/// 字符串白名单（OFFICE / PRODUCTION_SHELF / WORKER / INSPECTION_SHELF /
/// OUTSOURCE_COMPANY）；`holder_ids` 是雪花 ID 字符串，service 层 deserialize 成
/// `Vec<i64>` 后查 `t_part_batch.current_holder_id`（多态：t_shelf /
/// t_worker / t_outsource_company 任一表匹配即命中）。
/// 两者均查 `t_part_batch`（真相源在 batch；t_part 已无 location /
/// current_holder_id 列），按 part 下任意 active batch 命中即返。
/// `sort_by` 白名单（CREATED_AT / UPDATED_AT / PLANNED_DELIVERY_DATE /
/// REQUEST_DATE / SERIAL_NO / DRAWING_NO / NAME），其它退化为 `CREATED_AT`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PartListQuery {
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub statuses: Option<String>, // 逗号分隔字符串（query string 不支持 Vec 友好）
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub keyword: Option<String>,
    /// 2026-09-17 PR-4 修复：locations 逗号分隔字符串（query string 不支持 Vec
    /// 友好，与 `statuses` 同形）。透传到 repo `PartListFilters.locations`，
    /// 查 `t_part_batch.location = ANY(...)`。
    #[serde(default)]
    pub locations: Option<String>,
    /// 2026-09-17 PR-4 修复：holder_ids 逗号分隔雪花 ID 字符串。service 层 parse
    /// 成 `Vec<i64>`，透传到 repo `PartListFilters.holder_ids`，查
    /// `t_part_batch.current_holder_id = ANY(...)`（多态 holder：t_shelf /
    /// t_worker / t_outsource_company 任一匹配即命中）。
    #[serde(default)]
    pub holder_ids: Option<String>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>, // "ASC" / "DESC"
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub offset: Option<i64>,
}

// ===== Soft-delete =====

/// `POST /parts/{id}/soft-delete` 入参：`version` 必填（OCC）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartSoftDeleteRequest {
    pub version: i32,
}

// ===== Lifecycle =====

/// `POST /parts/{id}/deliver` 入参。
///
/// 2026-09-11 part/assembly/batch 重构方案 §4.3 (PR-B3) BREAKING CHANGE：
/// lifecycle 三端点（deliver / complete / start-repair）改为 batch 级，OCC
/// 锚定 `t_part_batch.version`。`batch_id` 由前端从
/// `GET /parts/by-serial/{serial_no}/part-batches` 拿到（每个 batch 含
/// `id` + `version` + `status`），状态机守卫读 batch 当前状态。
///
/// `batch_id` 用 String 序列化（雪花 i64 > 2^53，按 string 透传）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeliverRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/cancel` 入参。
///
/// cancel 保持 part 级（PR-B3 §4.3 D3）：级联取消全部活跃批次 → part 翻转
/// CANCELLED。`reason` 优先作为事件 note。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CancelRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/complete` 入参。
///
/// 2026-09-11 part/assembly/batch 重构方案 §4.3 (PR-B3) BREAKING CHANGE：
/// 收 `batch_id` + `version`（锚 `t_part_batch.version`，与 inspection 三流一致）。
/// 状态机守卫读 batch 当前状态 `DELIVERED → COMPLETED`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompleteRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/start-repair` 入参。
///
/// 2026-09-11 part/assembly/batch 重构方案 §4.3 (PR-B3) BREAKING CHANGE：
/// 收 `batch_id` + `version`。状态机守卫读 batch 当前状态
/// `IN_PROCESS → REPAIRING`。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：t_part_batch 删 `has_been_repaired` 列；
/// 返修事实改由 `t_part_event.event_type='REPAIR_STARTED'` +
/// `t_part_batch.status='REPAIRING'` 承担（part 派生列无需同步）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StartRepairRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

// ===== Phase 1（2026-09-13）14 端点 DTO =====

/// `POST /parts/{id}/place-on-shelf` 入参。
///
/// PENDING → IN_PROCESS（`location='PRODUCTION_SHELF'`）：放到指定生产货架。
/// 状态机守卫读 batch 当前状态 `PENDING → IN_PROCESS`；service 层校验
/// `shelf ↔ process` 映射（`BIZ_SHELF_PROCESS_NOT_MAPPED` 422）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlaceOnShelfRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub next_process_id: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/recall-to-pending` 入参。
///
/// ON_SHELF（IN_PROCESS+PRODUCTION_SHELF）或 PROGRAMMING → PENDING：
/// 召回未领批次回 PENDING。状态机白名单放行 `IN_PROCESS → PENDING` 与
/// `PROGRAMMING → PENDING`；service 层守 `IN_PROCESS` 时必须有 `location=PRODUCTION_SHELF`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallToPendingRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/send-to-programming` 入参。
///
/// PENDING → PROGRAMMING（`location='OFFICE'`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendToProgrammingRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/release-from-programming` 入参。
///
/// PROGRAMMING → IN_PROCESS（`location='PRODUCTION_SHELF'`）。复用
/// `PlaceOnShelfRequest`（shelf_id + next_process_id + 校验 shelf↔process 映射）。
pub type ReleaseFromProgrammingRequest = PlaceOnShelfRequest;

/// `POST /parts/{id}/recall-to-programming` 入参。
///
/// IN_PROCESS+PRODUCTION_SHELF → PROGRAMMING（召回已下发未领的工件至编程）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallToProgrammingRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/send-to-outsource` 入参。
///
/// PENDING / IN_PROCESS+PRODUCTION_SHELF → OUTSOURCE（`location='OUTSOURCE_COMPANY'`）。
/// `outsource_company_id` + `process_id` 必填。Phase 1 简化版：状态机放行
/// `PENDING → OUTSOURCE` 与 `IN_PROCESS → OUTSOURCE`（白名单后者由
/// `IN_PROCESS→PROGRAMMING` 已有的位置守 + service 层新增的 `OUTSOURCE` 目标组合守卫）。
///
/// Phase 2（2026-09-13）：
/// - `quote_id` 必填（APPROVED 报价）；service 在同事务内 INSERT t_outsource_shipment
/// - `direct` 标志：当 `direct=true` 时跳过 quote 校验（DIRECT 免审批占位；
///   Phase 2 stub：返回 501 NOT_IMPLEMENTED）
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendToOutsourceRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub outsource_company_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub process_id: i64,
    /// APPROVED 报价 id（DIRECT 模式 stub 时必填；service 内会校验）
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub quote_id: Option<i64>,
    /// DIRECT 模式占位（暂返回 501 NOT_IMPLEMENTED；follow-up 任务）
    #[serde(default)]
    pub direct: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/receive-from-outsource` 入参。
///
/// OUTSOURCE → IN_PROCESS（回生产架）。复用 `PlaceOnShelfRequest` 形态
/// （shelf_id + next_process_id）。
pub type ReceiveFromOutsourceRequest = PlaceOnShelfRequest;

/// `POST /parts/{id}/receive-from-outsource-to-inspection` 入参。
///
/// OUTSOURCE → INSPECTION（直接送检）。`shelf_id` 必填，service 层校验 zone=INSPECTION。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReceiveFromOutsourceToInspectionRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(default)]
    pub auto_pass_inspection: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/complete-repair` 入参。
///
/// REPAIRING → IN_PROCESS（落回生产架）或 REPAIRING → INSPECTION（送检区）。
/// shelf.zone=PRODUCTION 时 next_process_id 必填且需校验 shelf↔process 映射。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompleteRepairRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub next_process_id: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/repair-dispatch` 入参。
///
/// 一步式返修下发（`start_repair + complete_repair` 合并）：从 IN_PROCESS / INSPECTION
/// / READY_TO_SHIP 入口直达目标状态。`shelf_id` 必填（PRODUCTION 或 INSPECTION 区）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RepairDispatchRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
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

/// `POST /parts/{id}/batches/split` 入参。
///
/// 拆出部分量为新批次（继承源批次 status/location/holder/next_process；
/// 不继承 delivery_note_id）。`quantity` ∈ [1, source_batch.quantity - 1]。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SplitBatchRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub quantity: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/{id}/pick-up` 入参（B 方案：手动 pick-up 兜底）。
///
/// PENDING / IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER。
/// `worker_id` 必填（持有件工人）；`batch_id` 必填；`shelf_id` 必填
/// （当前批次所在货架；service 层仅校验存在 + 同 shelf ↔ process 映射）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PickUpRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `GET /parts/by-work-type/{work_type_id}` 入参（query）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ByWorkTypeQuery {
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub shelf_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `GET /parts/pickable-by-work-type/{work_type_id}` 入参（query）。
pub type PickableByWorkTypeQuery = ByWorkTypeQuery;

/// `GET /parts/by-worker/{worker_id}` 入参（query）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ByWorkerQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `POST /parts/{id}/batches/{batch_id}/cancel` 入参。
///
/// 批次级取消：终态保护，非终态 → CANCELLED。`version` OCC 守。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CancelBatchRequest {
    pub version: i32,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /parts/{id}/scan-inspect` 入参。
///
/// 扫码快捷品检（一步式：`{PENDING, PROGRAMMING, IN_PROCESS}` → INSPECTION →
/// READY_TO_SHIP 或 REPAIRING，由 `pass` 字段决定）。
/// `target_inspection_shelf_id` 必填（INSPECTION 区 active）。
/// `pass=true`：READY_TO_SHIP；`pass=false`：REPAIRING + 需要 `shelf_id` +
/// `next_process_id`（落回生产架用）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScanInspectRequest {
    pub pass: bool,
    #[serde(deserialize_with = "deserialize_i64")]
    pub target_inspection_shelf_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub shelf_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub next_process_id: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/scan/deliver-part` 入参（无 path part_id；从 `serial_no` 反查）。
///
/// 司机扫码发货：`part_serial_no` + `worker_badge_code`。Service 层校验
/// worker.work_type.code == '送货司机'。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScanDeliverPartRequest {
    pub part_serial_no: String,
    pub worker_badge_code: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/batch-with-pdfs` multipart 入参：JSON + PDFs。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchWithPdfsRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub customer_id: i64,
    #[serde(default)]
    pub applicant_name: Option<String>,
    #[serde(default)]
    pub request_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/match-by-excel-items` 入参：Excel 行（drawing_no 或 serial_no）→ 现有 part id。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MatchByExcelItemsRequest {
    pub items: Vec<MatchByExcelItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MatchByExcelItem {
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub serial_no: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// `POST /parts/batch-update-order-info` 入参。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchUpdateOrderInfoRequest {
    pub items: Vec<BatchUpdateOrderInfoItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchUpdateOrderInfoItem {
    #[serde(deserialize_with = "deserialize_i64")]
    pub part_id: i64,
    pub version: i32,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
}