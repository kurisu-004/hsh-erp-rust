//! part 域 CRUD + lifecycle DTO（Phase PR-CRUD 2026-08-25）
//!
//! 命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxOut`：单条详情出参（id 字段用 `#[serde(serialize_with =
//!   shared::types::serialize_i64)]`）
//! - `XxxListItem` / `XxxListOut`：列表分页
//! - `XxxListQuery`：列表查询参数

use serde::{Deserialize, Serialize};

use crate::modules::part::model::TPart;
use crate::shared::types::{
    deserialize_i64, deserialize_i64_opt, serialize_i64, serialize_i64_opt,
};

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

/// `POST /parts/batch` per-item 失败明细。
///
/// `part_id`：`Some(id)` = INSERT 成功但 detail lookup 失败；
///            `None` = INSERT 本身失败。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchCreateFailure {
    #[serde(serialize_with = "serialize_i64_opt")]
    pub part_id: Option<i64>,
    pub code: i32,
    pub message: String,
    pub item_index: usize,
}

/// `POST /parts/batch` 出参：`created` 与 `failed` 互斥。
///
/// 2026-09-16 M2-B review 第 1 轮：`cleanup_tmp_keys` 新增字段。
/// - 含义：本批次成功 INSERT 后、需要 commit 后异步清理的 tmp 对象 key 列表
///   （client 已直传到 COS tmp 区，已被 service 端 head+copy 到 CAS key）。
/// - 用途：handler 在 `tx.commit()` 之后 `tokio::spawn` 批量 `cos.delete_object(&key)`
///   兜底，避免 commit 失败却已触发 COS 删除产生孤儿。
/// - 前端不需要该字段（`#[serde(default)]` 兜空，前端忽略）；后端用 `out.cleanup_tmp_keys`。
/// - legacy（无 binding）路径该列表为空，前端 / 集成测试无需关注。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchCreateOut {
    pub created: Vec<PartDetailOut>,
    pub failed: Vec<PartBatchCreateFailure>,
    /// commit 后由 handler spawn 异步清理的 tmp 对象 key 列表。
    #[serde(default)]
    pub cleanup_tmp_keys: Vec<String>,
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
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>, // "ASC" / "DESC"
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub offset: Option<i64>,
}

/// `GET /parts` 列表行：`TPart` + 客户冗余字段 + 派生位置 / 持有人。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：t_part 不再持有 `location` /
/// `current_holder_id`（已删列），前端列表需要的「位置 / 持有人」展示由
/// service 层在 `list_parts` 内按 min-progress 活跃批次派生（见
/// `PartService::list_parts` 内的 batch enrichment 段）。
///
/// 派生规则：
/// - `location`：该 part min-progress 活跃批次（与 `compute_part_target` 一
///   致；非 CANCELLED 非 COMPLETED 批次中 progress 最小者）的 `location`；
///   无活跃批次 → `None`。
/// - `holder_name`：同批次 `current_holder_id` 解析的展示名称；按
///   `batch.location` 分桶：
///   - `PRODUCTION_SHELF` / `INSPECTION_SHELF` → `t_shelf.code`
///   - `WORKER` → `t_worker.name`
///   - `OUTSOURCE_COMPANY` → `t_outsource_company.name`
///   - `OFFICE` / `NULL` / 无活跃批次 → `None`
#[derive(Debug, Clone, Serialize)]
pub struct PartListItem {
    #[serde(flatten)]
    pub part: TPart,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
    /// 派生位置（见字段级 doc 注释）。
    #[serde(default)]
    pub location: Option<String>,
    /// 派生持有人名称（见字段级 doc 注释）。
    #[serde(default)]
    pub holder_name: Option<String>,
}

/// `GET /parts` 出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct PartListOut {
    pub items: Vec<PartListItem>,
    #[serde(serialize_with = "serialize_i64")]
    pub total: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub limit: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub offset: i64,
}

// ===== Detail =====

/// `POST /parts` / `GET /parts/{id}` 出参：完整工单 + 客户冗余字段 +
/// 当前 INSPECTION 批次 id（前端轮询用；`None` 表示当前不在 INSPECTION）。
#[derive(Debug, Clone, Serialize)]
pub struct PartDetailOut {
    #[serde(flatten)]
    pub part: TPart,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_batch_id: Option<i64>,
}

impl PartDetailOut {
    /// 由完整 `TPart` + 客户冗余字段 + 当前 INSPECTION 批次 id 构造。
    ///
    /// `current_batch_id` 由 service 层调用
    /// [`crate::modules::part::repo::PartRepo::find_current_inspection_batch_id`]
    /// 取值；`None` 表示当前不在 INSPECTION。
    pub fn from_with_customer_extra(
        part: TPart,
        current_batch_id: Option<i64>,
        customer_name: Option<String>,
        l1_customer_name: Option<String>,
    ) -> Self {
        Self {
            part,
            customer_name,
            l1_customer_name,
            current_batch_id,
        }
    }
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

/// `POST /parts/match-by-excel-items` 出参：单行匹配结果（part_id 或 null）。
#[derive(Debug, Clone, Serialize)]
pub struct MatchByExcelItemResult {
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub serial_no: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub part_id: Option<i64>,
    pub status: String, // "MATCHED" / "NOT_FOUND" / "AMBIGUOUS"
    #[serde(default)]
    pub message: Option<String>,
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

/// `POST /parts/batch-update-order-info` 出参：成功 N，失败列表。
#[derive(Debug, Clone, Serialize)]
pub struct BatchUpdateOrderInfoOut {
    pub updated: i64,
    pub failed: Vec<BatchUpdateOrderInfoFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchUpdateOrderInfoFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub code: i32,
    pub message: String,
}

/// `GET /parts/{id}/events` 出参：工单事件日志列表（按 created_at 倒序）。
#[derive(Debug, Clone, Serialize)]
pub struct PartEventOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub event_type: String,
    #[serde(default)]
    pub from_status: Option<String>,
    #[serde(default)]
    pub to_status: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub batch_id: Option<i64>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub drawing_code: Option<String>,
    #[serde(default)]
    pub badge_code: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub created_by: Option<i64>,
}

/// `GET /parts/{id}/batches` 出参：工单全部活跃批次 + holder 名称解析。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `has_been_repaired` 字段
/// （t_part_batch 列已删；返修事实由 t_part_event REPAIR_STARTED 事件追溯）。
///
/// 2026-09-16 PR-3 批次 step 化（migration 028）：
/// - 删 `placed_at`（t_part_batch 列已删，不再统计生产时间）
/// - `next_process_id` 字段保留（DTO 兼容），但 service 层不再写入；保留仅作
///   历史快照语义，**禁止**新端点写入该字段
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchListItemOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_holder_id: Option<i64>,
    #[serde(default)]
    pub holder_name: Option<String>,
    /// 2026-09-16 PR-3：DTO 保留字段名（兼容前端），但当前**全部为 None**——
    /// 业务上「下一步工序」概念已迁移到 step（current_process_step_id →
    /// JOIN step.process_id 派生）；新端点不应依赖该字段。如前端仍需该信息，
    /// 由 frontend 自行 JOIN current_process_step_id → step.process_id。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub delivery_note_id: Option<i64>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub parent_batch_id: Option<i64>,
    pub version: i32,
}

/// `GET /parts/location-tree` 出参：按 shelf/status 聚合的位置树。
#[derive(Debug, Clone, Serialize)]
pub struct LocationTreeNodeOut {
    pub id: String,
    pub label: String,
    pub kind: String, // "OFFICE" / "PRODUCTION_SHELF" / "WORKER" / "INSPECTION_SHELF" / "OUTSOURCE_COMPANY"
    #[serde(serialize_with = "serialize_i64_opt")]
    pub parent_id: Option<i64>,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocationTreeOut {
    pub items: Vec<LocationTreeNodeOut>,
}

/// `GET /parts/pending-programming` 出参：PROGRAMMING 状态工单一览（复用 PartListOut）。
pub type PendingProgrammingOut = crate::modules::part::dto_crud::PartListOut;

/// `GET /parts/repair-batches` / `repairing-batches` 出参：返修批次列表（复用 InspectionBatchListOut）。
pub type RepairBatchesOut = crate::modules::part::dto::InspectionBatchListOut;
