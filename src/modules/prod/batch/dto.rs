//! prod::batch 子模块 DTO —— 入参 + 校验
//!
//! 2026-10-08：下发流的 5 个入参（`ListPendingQuery` / `DispatchRequest` /
//! `DispatchTarget` / `AutoDispatchRequest` / `RecallToPendingRequest`）与对应
//! 出参一起迁往 `prod::queue::dto` —— 它们的唯一消费方是队列页的下发 / 召回动作。
//!
//! 与 queue / process_chain 等同形 DTO 模块，
//! 仅入参（`Serialize` + 反序列化兜底由 axum `Json` extractor 处理）。
//! 出参结构见 [`super::vo`]。
//! i64 反序列化兜底走 `deserialize_i64` / `deserialize_i64_opt`（与其它域惯例一致：
//! 只接受 JSON 字符串形态，雪花 ID 一律 string 以避免 JS `Number.MAX_SAFE_INTEGER`
//! 精度截断；发数字会在 axum `JsonRejection` 层被拒 —— HTTP 422 纯文本、不进
//! `R<T>` 信封）。**计数字段刻意不套这层兜底**（`version` / `quantity` 走裸 JSON
//! 数字），见 [`SplitBatchByBodyRequest`]。
//! 2026-10-02 追加：自 part 域迁入批次流转入参（见文件末尾小节）。
//!
//! 2026-10-10：报工台的 `WorkerScanRequest` / `PickUpRequest`（连同
//! `WorkerScanEvent` 枚举）迁往 `crate::modules::prod::scan::dto`。

use serde::Deserialize;

use crate::shared::types::{deserialize_i64, deserialize_i64_opt};

// 2026-10-02：自 part 域迁入的批次流转 DTO（原 `part/dto.rs` + `part/dto_crud.rs`）
// ============================================================================
//
// 2026-10-02：迁入的根因
//
// 这些入参全部是**以批次为操作对象**的端点（OCC 锚 `t_part_batch.version`），
// 2026-10-02 起 URL 从 `POST /api/v2/parts/{part_id}/…` 硬切到
// `POST /api/v2/prod/batches/{batch_id}/…`，故 DTO 随 handler 一并迁入 prod 域。
// ## 契约变更：子资源的 `batch_id` 字段**删除**
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
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 40901。
///
/// ## `target_inspection_shelf_id` 已移除（2026-10-10）
/// 目标品检架由服务端按负载自动选（`shared::shelf::select::pick_least_loaded`），
/// 选不出时返 `40301 SHELF_MISMATCH`（scope 内无可用品检架）。
/// 老客户端多发的 `shelf_id` 会被 serde 静默忽略（本仓生产代码零
/// `deny_unknown_fields`），故移除是**向后兼容**的；但新客户端发老版本服务端
/// 会得 422（字段必填缺失），**部署顺序必须后端先上**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToInspectionRequest {
    pub version: i32,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// `POST /api/v2/prod/batches/{batch_id}/to-process` 入参。
///
/// 状态机迁移：`INSPECTION` → `IN_PROCESS`，同时写入目标 production shelf。
/// `next_process_id`：**必填**且保留 —— 工人指定的是「打回到哪道工序」不是「打回到
/// 哪个架」，服务端按这道工序 + 负载自动挑架（候选集只含映射了该工序的活跃生产架）。
/// `version`：**必填**；目标批次 `t_part_batch.version`；不符 → 40901。
///
/// ## `shelf_id` 已移除（2026-10-10）
/// 老客户端多发的 `shelf_id` 会被 serde 静默忽略（本仓生产代码零
/// `deny_unknown_fields`），故移除是**向后兼容**的；但新客户端发老版本服务端
/// 会得 422（字段必填缺失），**部署顺序必须后端先上**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToProcessRequest {
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
/// `items.len()` 限制由 service 校验（`BATCH_TO_INSPECTION_MAX_ITEMS`）。
/// 品检架由服务端选**一次**、这批 item 共用（与单件端点同款选架，但不在循环内选）。
///
/// ## `target_inspection_shelf_id` 已移除（2026-10-10）
/// 移除与单件端点同款：向后兼容（老客户端多发的字段被 serde 静默忽略），但
/// **部署顺序必须后端先上**（新客户端发老服务端会得 422）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchToInspectionRequest {
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

/// `GET /api/v2/prod/batches/repair` / `repairing` 查询参数（2 条共用）。
///
/// **仅这 2 条端点**：待品检队列读（`GET /api/v2/prod/inspection/queue`）的查询
/// 参数在 `prod::inspection` 域自己的 dto 模块里，与本结构无关。
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
/// 被 2 个端点复用（place-on-shelf / release-from-programming）。
///
/// ⚠️ 2026-10-09：第三个复用方 `receive-from-outsource` 随外协三合一迁移到
/// `POST /api/v2/outsource-queue/move`（入参改为 `to: {kind: PRODUCTION_SHELF,
/// shelf_id, next_process_id}`），本结构不再是 3 复用。
///
/// PENDING → IN_PROCESS（`location='PRODUCTION_SHELF'`）：放到服务端选的架上。
///
/// `next_process_id` 必填 —— **服务端按这道工序选架**，候选集只含
/// `t_shelf_process` 里映射了该工序的活跃生产架，因此原「`shelf ↔ process` 映射
/// 校验（`BIZ_SHELF_PROCESS_NOT_MAPPED` 422）」已被选架本身覆盖；选不出时返
/// `20508 BIZ_SHELF_PROCESS_NOT_FOUND`。
///
/// ## `shelf_id` 已移除（2026-10-10）
/// 老客户端多发的 `shelf_id` 会被 serde 静默忽略（本仓生产代码零
/// `deny_unknown_fields`），故移除是**向后兼容**的；但新客户端发老版本服务端
/// 会得 422（字段必填缺失），**部署顺序必须后端先上**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlaceOnShelfRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub next_process_id: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/complete-repair` 入参。
///
/// 要求 batch `is_repairing = true`（确实在返修中）。
///
/// ## 去向由 `next_process_id` 有无决定（2026-10-10）
///
/// - `Some(np)` → 回生产：服务端按该工序自动选生产架（`current_load / capacity`
///   升序），写 `next_process_id` + step，目标 `IN_PROCESS`；
/// - `None` → 回品检：服务端自动选品检架（品检架无工序映射，故不按工序筛），
///   目标 `INSPECTION`。
///
/// 两条路径都清 `is_repairing`。
///
/// ## `shelf_id` 已移除（2026-10-10）
/// 它曾是「去向」的唯一载体（读 `shelf.zone` 分流）；现在 `next_process_id` 承担
/// 了这个语义 —— 工人填「打回到哪道工序」就是「回生产」，不填就是「回品检」。
/// 老客户端多发的 `shelf_id` 会被 serde 静默忽略（本仓生产代码零
/// `deny_unknown_fields`），故移除本身**向后兼容**；但去向**会漂**，两个方向都要注意：
///
/// | 老 body | 老服务端 | 新服务端 |
/// |---|---|---|
/// | 生产架 + `next_process_id` | 回生产 | 回生产 ✅ |
/// | 生产架 + 无 `next_process_id` | `20104` | **回品检**（静默改去向） |
/// | 品检架 + `next_process_id` | 回品检（该字段被忽略） | **回生产**（静默改去向）⚠️ |
/// | 品检架 + 无 `next_process_id` | 回品检 | 回品检 ✅ |
///
/// ⚠️ 第三行是更危险的一侧：老服务端在品检分支完全忽略 `next_process_id`，所以
/// 「回品检时顺带把下一道工序一起发过去」在老系统里是无感的，新服务端会当成「回生产」
/// —— 把本该返修完送检的货放回产线重跑，且返回 200。
/// ⇒ **调用方义务**：回生产时发 `next_process_id`，**回品检时不得发**。
/// 部署顺序必须后端先上。逐端点登记见 [`RepairDispatchRequest`] 与
/// `docs/api/batch.md` §2.1。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompleteRepairRequest {
    pub version: i32,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub next_process_id: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/batches/{batch_id}/repair-dispatch` 入参。
///
/// 一步式返修下发（`start_repair + complete_repair` 合并）：从 IN_PROCESS / INSPECTION
/// / READY_TO_SHIP 入口直达目标状态。
///
/// 去向由 `next_process_id` 有无决定（与 [`CompleteRepairRequest`] 同款）：
/// `Some` → 回生产（自动选生产架）、`None` → 回品检（自动选品检架）。
///
/// ## `shelf_id` 已移除（2026-10-10）
/// 移除理由与向后兼容性同 [`CompleteRepairRequest::shelf_id`]。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RepairDispatchRequest {
    pub version: i32,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub next_process_id: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/batches/split` 入参。
///
/// 拆出部分量为新批次（继承源批次 status/location/holder/next_process；
/// 不继承 delivery_note_id）。`quantity` ∈ [1, source_batch.quantity - 1]。
///
/// 2026-10-09 新增：`batch_id` 由路径参数改入 body（旧路径
/// `POST /api/v2/prod/batches/{batch_id}/split` 已硬切下线、无 alias）——
/// 本端点有**三个消费方**（生产队列看板 / 外协看板 / 零件详情页），提到顶层
/// `/api/v2/batches` 之后不能再按 `/{batch_id}/…` 形状挂载，且这与
/// `prod::queue::recall` 的硬切同款（ID 全走 body）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SplitBatchByBodyRequest {
    /// 源批次雪花 ID。**必须发 JSON 字符串**（`deserialize_i64` 只吃 `str`），
    /// 例如 `"batch_id": "1590000000000000001"`；发数字 → axum `Json` 提取器
    /// 直接拒 ⇒ **HTTP 422 纯文本、不进 `R<T>` 信封**（响应里没有 `code` 字段）。
    ///
    /// 对照：同结构的 `version` / `quantity` 是计数与 OCC 锚，走**裸 JSON 数字**。
    /// 同一份请求体里字符串 / 数字混用是刻意的，不要互相套用。
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    /// `t_part_batch.version` 的 OCC 锚，**必填**（无 `#[serde(default)]`）：缺字段
    /// → 422 纯文本；与库中当前值不符 → `40901 VERSION_CONFLICT`。
    pub version: i32,
    /// 拆出数量，∈ [1, 源批次 quantity - 1]。
    ///
    /// **裸 JSON 数字**（不带 `deserialize_i64`）—— 数量是 i32 量级的计数，
    /// `deserialize_i64` 是给 19 位雪花 ID 防 JS 精度截断用的，挂在这里会让前端发数字
    /// 直接吃 axum `Json` 提取器的 422 纯文本（不是业务信封，前端无从提示原因）。
    pub quantity: i32,
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

/// `POST /api/v2/prod/batches/{batch_id}/scan-inspect` 入参。
///
/// 扫码快捷品检（一步式：`{PENDING, PROGRAMMING, IN_PROCESS}` → INSPECTION →
/// READY_TO_SHIP 或「返修中」，由 `pass` 字段决定）。
///
/// `pass=false`：`status='IN_PROCESS'` + `is_repairing=true`（批次停在送检架；
/// **落回生产架要打哪道工序**改由随后的 `complete-repair` 自己带 `next_process_id`
/// 决定，本端点不再预收）。
///
/// ## 三个字段已移除（2026-10-10）
///
/// - `target_inspection_shelf_id`：目标品检架由服务端按负载自动选；
/// - `shelf_id` / `next_process_id`：**前向兼容字段，本端点从 2026-10-04 起就从不
///   消费**（它们的 doc 当时明写「供随后的 complete-repair 用」）。自动选架让
///   「先告诉 complete-repair 用哪个架」这件事彻底没有意义，故一并删掉，避免调用
///   方以为它们生效。
///
/// 三个字段的移除都是**向后兼容**的（老客户端多发的字段被 serde 静默忽略，本仓
/// 生产代码零 `deny_unknown_fields`）；但新客户端发老版本服务端会得 422，
/// **部署顺序必须后端先上**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScanInspectRequest {
    pub pass: bool,
    pub version: i32,
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
