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
///
/// `shelf_id`：**必填，且两个 event_type 都是 PRODUCTION 区的 worker-scan 货架** ——
/// service 对它做的是**无条件**的 PRODUCTION 硬校验（不分 `event_type`），非
/// PRODUCTION → 20501 `BIZ_SHELF_NOT_FOUND`。**不要**把 INSPECTION 区的品检架塞进
/// 本字段：INSPECTED 的品检架走 `target_inspection_shelf_id`。任何声称本字段
/// 「按 `event_type` 分支校验 zone」的说明都是错的 —— 本段是唯一权威口径。
///
/// 它**必须留在 PRODUCTION 区**的真正原因不是「工人站在哪个架前」：INSPECTED 时它
/// 是**补料用的生产架**，在同事务的 worker-pool refill 里当候选池的
/// `current_holder_id` 过滤键用（候选池 SQL 限
/// `location='PRODUCTION_SHELF' AND current_holder_id = $2`，见
/// `prod/worker_pool/repo/sql.rs`）。传品检架会让 refill 查空池。
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

/// `GET /api/v2/prod/batches/repair` / `repairing` 查询参数（2 条共用）。
///
/// **仅这 2 条端点**（2026-10-03 起）：`GET /prod/batches/inspection` 已分化到
/// [`InspectionQueueQuery`]。
///
/// `customer_id` 单值；service 层复用 `expand_customer_id` 展开为 L1+L2 ids
/// （与 `list_parts` 同逻辑）。`keyword` / `serial_no` ILIKE 匹配。
/// `planned_delivery_date_*` 作用于 `t_part.planned_delivery_date`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RepairBatchListQuery {
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

/// `GET /api/v2/prod/batches/inspection` 查询参数。
///
/// 2026-10-03 与 [`RepairBatchListQuery`] 分化：待品检页的筛选收敛到表头 7 列，
/// 每列一个独立参数（图号 / 名称 / 序列号各一个 ILIKE），不再用跨字段 `keyword`；
/// 日期区间筛系统交期（页面已不显示计划交期）。
/// `sort_by` 白名单（SERIAL_NO / DRAWING_NO / NAME / BATCH_NO / QUANTITY /
/// SYSTEM_DELIVERY_DATE / CUSTOMER_NAME），非法值退化为 SYSTEM_DELIVERY_DATE；
/// `sort_dir` 非法退化为 ASC。
/// 3 个文本筛选值含 `%` / `_` / `\` 时在 service 层拒（40001）—— 那是防 `%…%`
/// 被当通配符放大成全表扫描的**语义**约束，注入面由 repo 的 `push_bind` 保证。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InspectionQueueQuery {
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub serial_no: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub system_delivery_date_from: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub system_delivery_date_to: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>,
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
/// 被 3 个端点复用（place-on-shelf / release-from-programming /
/// receive-from-outsource）。
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
/// 2026-10-06 订正：`IN_PROCESS` + `location ∈ {PRODUCTION_SHELF, WORKER}` 或
/// `PROGRAMMING` → `PENDING`：召回已下发批次（含工人持有中的）回待下发池。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallToPendingRequest {
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/send-to-outsource` 入参。
///
/// PENDING / IN_PROCESS+PRODUCTION_SHELF → OUTSOURCE（`location='OUTSOURCE_COMPANY'`）。
/// `outsource_company_id` + `process_id` 必填。
///
/// ## 价来源：APPROVAL 与 DIRECT 二选一（2026-10-03 起强制）
///
/// - APPROVAL 模式：传 `quote_id`（APPROVED 报价）；service 在同事务内 INSERT
///   `t_outsource_shipment`，`unit_price = quote.price`。
/// - DIRECT 模式（免审批直发）：传 `direct = true`。service 先按
///   `(part_id, outsource_company_id, process_id)` 找活跃 APPROVED 报价复用；找不到
///   则自动建一条 `price = 0` 的 APPROVED 占位报价（`is_direct = true`），
///   shipment 的 `unit_price` 因而可能是 0 —— 对账时靠该报价的 `note` 识别。
/// - 两者都不传 / 同时传 → `400 BIZ_INVALID_VALUE`（service 层显式校验）。守卫的
///   必要性：没有价来源时 shipment 的 `unit_price` 只能是 0，外协对账页会看到
///   「单价 0」却无从判断是漏填还是免审批直发。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendToOutsourceRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub outsource_company_id: i64,
    /// 外协加工的**工序** id（`t_process.category` 必须为 `'OUTSOURCE'`，且该公司
    /// 必须映射该工序）。
    ///
    /// 字段名是 `process_id` 而非 `next_process_id`：本名与已上线的 Python v1 客户端
    /// 绑定，改名会破坏它。前端已按本名对齐（`SendToOutsourcePayload.process_id`，
    /// 契约用例逐字钉死并反断言 body 里不得出现 `next_process_id`）。
    ///
    /// ⚠️ 本字段**无 `serde(default)`**，是必填；本结构也**没有**
    /// `deny_unknown_fields`，故发 `next_process_id` 会被静默丢弃并因必填字段缺失
    /// 而失败：**`422` + 纯文本** `missing field \`process_id\``（axum `Json` 提取器），
    /// 不是业务信封、不要按 `BIZ_PROCESS_NOT_FOUND` 排查。刻意不加
    /// `deny_unknown_fields`：那会让任何多余字段直接 422，迁移面远大于收益。
    /// 契约见 `docs/api/production/batches.md#外协流转send--receive`。
    #[serde(deserialize_with = "deserialize_i64")]
    pub process_id: i64,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub quote_id: Option<i64>,
    #[serde(default)]
    pub direct: Option<bool>,
    /// 部分发送数量（2026-10-03 新增）。
    ///
    /// - `None` 或 `== 批次量` = 整批发送（不拆批）。
    /// - `0 < q < 批次量` = **部分发送**：先把源批次按 `q` 拆出新子批次，只把子
    ///   批次发出（源批次留在原货架、状态不变、量减少 `q`），shipment 的
    ///   `quantity` 记 `q`。
    /// - `q <= 0` 或 `q > 批次量` → `400 BIZ_INVALID_VALUE`。
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource` 入参。
///
/// OUTSOURCE → IN_PROCESS（`location='PRODUCTION_SHELF'`，`next_process_id` 为收回后
/// 重新入池的工序）。
///
/// 2026-10-03 从 `PlaceOnShelfRequest` 独立出来：外协收回要支持**部分接收**
/// （`quantity`），而 `PlaceOnShelfRequest` 仍被 `place-on-shelf` /
/// `release-from-programming` 两个端点共用，加字段会污染它们的契约。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReceiveFromOutsourceRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,
    #[serde(deserialize_with = "deserialize_i64")]
    pub next_process_id: i64,
    /// 部分接收数量（2026-10-03 新增），语义同
    /// [`SendToOutsourceRequest::quantity`]。
    ///
    /// ⚠️ 记账口径：部分接收**只拆批**、不动 shipment —— 源批次保留余量且**开口
    /// shipment 保持 `OUTSOURCING`**，`received_at` / `status='RECEIVED'` 只在
    /// 整批回收时才落。故 `shipment.quantity` 与当前批次余量可能不相等，这是有意的。
    #[serde(default)]
    pub quantity: Option<i32>,
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
/// `worker_id` 必填（持有件工人）；`shelf_id` **可选**（见该字段 doc）。
///
/// 2026-10-03 新增：部分领取。`quantity` 缺省 = 整批领取（保持既有行为，向后
/// 兼容）；`0 < quantity < batch.quantity` 时 service 自动拆批，把拆出来的那
/// 部分交给工人，源批次留在原处、数量递减。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PickUpRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    /// 当前批次所在货架。**2026-10-04 起可选**。
    ///
    /// ## 传了才校验，缺省什么都不做
    ///
    /// `Some(sid)` → service 校验「存在 + `is_active` + `zone='PRODUCTION'`」
    /// （`validate_shelf_zone`，依次 `20501` / `20512` / `20104`）。
    /// `None`（缺省或显式 `null`）→ **不校验、不推导、不回退**，请求照常受理。
    ///
    /// ## 为什么可以缺省：这个字段对最终结果零影响
    ///
    /// pick-up 路径上 `shelf_id` 只进 `validate_shelf_zone`，而它内部只
    /// `SELECT ... FROM t_shelf WHERE id = $1 AND deleted_at IS NULL`（零写）；
    /// 本路径 `t_part_batch` 的全部 3 个写入点（拆成 4 条 SQL；`pickup.rs` 内联
    /// SQL、`guard.rs` → `status_gate.rs` 的通用 UPDATE、部分领取的
    /// `split_batch_for_partial_pass` = `_split_batch_inner` 的 INSERT + UPDATE）
    /// 的 SET 与 WHERE 均无货架列或货架条件；
    /// `t_part_event` 无货架列；响应 VO `PartOut` 无 shelf 字段。
    /// ⇒ 那条校验是**防呆断言**（让手填错区的人当场看见 20104），不是安全边界，
    /// 故不必强绑在成功路径上 —— 扫码台 / 看板等自动发起 pick-up 的调用方
    /// 本就无从知道「批次此刻名义上在哪一个架」。
    ///
    /// ## 订正一处旧表述（2026-10-04）
    ///
    /// 本字段旧注释写「service 层仅校验存在 + 同 shelf ↔ process 映射」，
    /// **后半句是错的**：pick-up 从不校验货架↔工序映射，
    /// `assert_shelf_maps_process`（`20507 BIZ_SHELF_PROCESS_NOT_MAPPED`）在本
    /// 路径一次都没被调用 —— 它只服务 `place-on-shelf` 与 worker-scan。
    ///
    /// ## 为什么不做「从 `current_holder_id` 推导」
    ///
    /// 技术上不可行（2026-10-04 逐条核实）：
    /// 1. PENDING 起点的批次 `current_holder_id` 恒为 `NULL`
    ///    （`create_initial_batch` 写死 `NULL, NULL`），而 PENDING 正是「待下发池」
    ///    的 pick-up 起点；
    /// 2. IN_PROCESS 起点只守 `location='PRODUCTION_SHELF'`、**不守 holder**，
    ///    `dispatch` 与 `pool/move` 两个写点能把 INSPECTION 区的架写进
    ///    `current_holder_id`；
    /// 3. `current_holder_id` 可能指向**已软删 / 非 PRODUCTION 区**的架，
    ///    「推导 + 施加同样校验」会把这类批次**永久锁死**。三条机制（2026-10-04
    ///    review 第 1 轮订正：原表述「货架被停用 / 软删时 holder 仍指向失效 id」
    ///    按字面不成立 —— `deactivate` 与 soft-delete 是同一操作，且 soft-delete
    ///    被引用时会被 `20503 BIZ_SHELF_IN_USE` 拦住）：
    ///    （a）`dispatch` 的 `ShelfProcessRepo::find_first_shelf_for_process` 只按
    ///    `t_shelf_process.deleted_at IS NULL` 过滤、**不 JOIN `t_shelf`** ⇒ 既不过滤
    ///    `zone` 也不过滤 `is_active`，映射残留时会把已软删的架 id 直接写进
    ///    `current_holder_id`；
    ///    （b）soft-delete 的 `20503` 守卫（`ShelfRepo::count_in_use_parts`）谓词是
    ///    `location IN ('PRODUCTION_SHELF','INSPECTION_SHELF') AND status IN
    ///    ('IN_PROCESS','INSPECTION')` ⇒ `location='PRODUCTION_SHELF'` 但 status 落在
    ///    该集合之外的行**不被计入**；
    ///    （c）该守卫是「先 count、再 soft_delete」两条独立语句、中间无锁 ⇒ 并发
    ///    上架可穿过守卫（TOCTOU）。
    ///
    /// ## ⚠️ 本字段**无 scope 校验**
    ///
    /// 与 worker-scan 对照：后者对 `shelf_id` 走 `can_access_shelf` 并在越权时
    /// 返 `40301 SHELF_MISMATCH`。pick-up 不做该校验，故本字段缺省时**没有**
    /// 任何货架维度的权限收敛；SHELF_ACCOUNT 角色门（`require_any_role`）是本
    /// 端点唯一的权限边界。
    ///
    /// 线上形态仍是 **JSON 字符串**（`deserialize_i64_opt` 只吃 `str`），
    /// 例如 `"shelf_id": "43"`；`"shelf_id": 43`（数字）→ `422`。
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub shelf_id: Option<i64>,
    /// 2026-10-03 新增：部分领取；缺省 = 整批。
    ///
    /// 线上形态与同族的 `SplitBatchRequest.quantity` 一致：**JSON 字符串**
    /// （`deserialize_i64_opt` 只吃 str），例如 `"quantity": "4"`。
    ///
    /// `None` / `>= batch.quantity` 一律按整批处理（`==` 是「显式整批」的合法
    /// 写法，语义与 `None` 等价）；`0 < q < batch.quantity` 触发自动拆批。
    /// 范围校验在 service 层，错误码 `BIZ_PART_BATCH_INVALID_QUANTITY`。
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub quantity: Option<i64>,
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
