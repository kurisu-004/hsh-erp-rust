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

use serde::Deserialize;

use crate::modules::prod::queue::dto::WorkerScanEvent;
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

/// `POST /api/v2/prod/batches/worker-scan` 入参。
///
/// 无 Path extractor：`serial_no` 是主键，`batch_id` 仅在多批次歧义时用于消歧。
/// `event_type`：`WorkerScanEvent::RETURNED` / `INSPECTED`。
///
/// ## `next_process_id`：**仅非顺应工序时必填**（2026-10-09 改写，2026-10-10 微调）
/// 后端先解析批次在工序链上的位置
/// （`shared::batch::chain::resolve_chain_position`，判据与读侧
/// `GET /parts/by-worker` 的 `chain_state` 逐条同源），按下表分流：
///
/// | 链上位置 | 分流 | `next_process_id` |
/// |---|---|---|
/// | `TAIL`（链内最后一道） | **自动送检**（2026-10-10 新增）：这批做完了，不落生产架 | 可省略 |
/// | `NEXT` 且顺应 | 后端按链推导下一道工序与 step | 可省略 |
/// | 其余（非顺应：无链 / 链已软删 / 指针漂移 / 链内 `process_id` 重复） | 按前端指定推进 | **必填**，缺失 → `40001` |
///
/// 字段类型保持 `Option<String>` 不变（不改 wire）：前端可以继续照 `chain_state`
/// 决定填不填，两条路径都合法。
///
/// ## ⚠️ 响应 `event_type` 可能与请求的**不同**（2026-10-10 新增）
/// 请求发 `RETURNED` 但批次在链尾时，服务端把它当送检处理，响应
/// `event_type = "WORKER_SCAN_INSPECTED"`。前端**必须按响应里的 `event_type` 分支**，
/// 不能按自己发的那一个 —— 服务端比前端更清楚批次做完了没有。语义与 WS 链路登记见
/// `docs/api/batch.md`。
///
/// ## 两个货架字段已移除（2026-10-10）
///
/// - `shelf_id`：目标生产架改由服务端按负载自动选
///   （`shared::shelf::select::pick_least_loaded`）。它原先的**双重**身份 —— 「放回
///   到的架」与「refill 的候选池过滤键」—— 两条都随之消失：放回由选架决定，补料改成
///   **跨全部映射该工种工序的活跃生产架**取料（`take_one_from_pool` 的
///   `shelf_id = NULL` 分支）；
/// - `target_inspection_shelf_id`：目标品检架同样由服务端按负载自动选。选不出时返
///   `40301 SHELF_MISMATCH`（当前账号 scope 内没有任何可用的 INSPECTION 架）。
///
/// 两个字段的移除都是**向后兼容**的（老客户端多发的字段被 serde 静默忽略，本仓生产
/// 代码零 `deny_unknown_fields`）；但新客户端发老版本服务端会得 422（`shelf_id`
/// 必填缺失），**部署顺序必须后端先上**。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerScanRequest {
    pub serial_no: String,
    pub badge_code: String,
    pub event_type: WorkerScanEvent,
    #[serde(default)]
    pub next_process_id: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
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
/// `deny_unknown_fields`），故移除是**向后兼容**的；但**新客户端必须重发
/// `next_process_id`**，否则「回生产」会被静默解释成「回品检」—— 这是本轮唯一
/// 一处**移除后语义会漂**的字段，部署顺序必须后端先上。
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
    /// SQL、`guards.rs` → `status.rs` 的通用 UPDATE、部分领取的
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
    /// 线上形态是 **JSON 字符串**（`deserialize_i64_opt` 只吃 `str`），例如
    /// `"quantity": "4"`；发数字 → 422 纯文本。本字段的字符串形态是**前端既有约定**
    /// （扫码台照此发送），与本模块其它「裸 JSON 数字计数」字段刻意不同形，改它要
    /// 连带改前端。
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
