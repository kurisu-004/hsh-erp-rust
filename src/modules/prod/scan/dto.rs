//! prod::scan DTO —— 全部入参（仅 `Deserialize`）
//!
//! 2026-10-10 自三个域搬入（按端点的前端消费方归位，不按后端逻辑相似度）：
//! - `prod::worker::dto::VerifyBadgeRequest`
//! - `part::dto_crud::{ByWorkTypeQuery, ByWorkerQuery}`（更名 `PickableQuery` /
//!   `HeldQuery`）
//! - `prod::batch::dto::{WorkerScanRequest, PickUpRequest}`
//! - `prod::queue::dto::WorkerScanEvent`（历史遗留：它是 worker-scan 的入参枚举，
//!   queue 只是当初收留了它；本轮按消费方归位）
//!
//! 2026-10-11：`WorkerScanRequest` 加 `quantity`（部分放回 / 部分送检）。它与同域
//! `PickUpRequest::quantity` **共用同一个 wire 形态与同一套三种落法**（见各自字段
//! doc），差别只有一处：worker-scan 的**余量留在工人手上**，pick-up 的余量留在原处。

use serde::Deserialize;

use crate::shared::types::{deserialize_i64, deserialize_i64_opt};

/// `POST /api/v2/prod/scan/verify-badge` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyBadgeRequest {
    pub badge_code: String,
}

/// `GET /api/v2/prod/scan/pickable` 查询参数。
///
/// ## 与旧 DTO 的两处形态变更
///
/// 1. `work_type_id` 由 **path 参数改 query 参数**（必填）—— 新路径是
///    `/scan/pickable?work_type_id=…`，与同域 `/scan/held?worker_id=…` 对称，
///    且两个 list 端点都是「过滤键 + 分页」的同款形态；
/// 2. **`shelf_id` 删除**。货架范围改由服务端按当前账号的 `shelf_ids` scope
///    收窄（`listing::service::pickable_shelf_scope`），而「指定某一个架」这件事
///    在 2026-10-10 起全仓再无写路径需要（8 条写路径的目标架都由
///    `shared::shelf::select::pick_least_loaded` 自动选）。保留一个无人发送的
///    客户端可控入参只会让人误以为「不传＝全给」。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PickableQuery {
    #[serde(deserialize_with = "deserialize_i64")]
    pub work_type_id: i64,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `GET /api/v2/prod/scan/held` 查询参数（`worker_id` 同样由 path 改 query、必填）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HeldQuery {
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `GET /api/v2/prod/scan/locate` 查询参数（2026-10-15 新增，扫码找件 · 诊断端点）。
///
/// - `serial_no` **必填**（图纸序列号）。缺省 → axum `QueryRejection` →
///   **HTTP 400 纯文本**；`trim()` 后为空串由 service 按「未命中」处理（`20101`）。
/// - `worker_id` **可选**：传了才计算可行动作 `action` / 失败原因
///   `disabled_reason`（针对该工人的可行动作），不传则两者恒 `null`（纯位置事实）。
///   线上形态是 **JSON 字符串**（`deserialize_i64_opt` 只吃 `str`，与同域
///   `HeldQuery.worker_id` / `PickUpRequest.quantity` 同款）；发数字 →
///   `QueryRejection` → **HTTP 400 纯文本**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LocateQuery {
    pub serial_no: String,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub worker_id: Option<i64>,
}

/// worker-scan 的动作类型（`RETURNED` 放回 / `INSPECTED` 直接送检）。
///
/// 2026-10-10 自 `prod::queue::dto` 搬来 —— 它是 `WorkerScanRequest` 的入参枚举，
/// 归属按**消费方**（报工台）判定，queue 域只是当初的收留地。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WorkerScanEvent {
    RETURNED,
    INSPECTED,
}

/// `POST /api/v2/prod/scan/worker-scan` 入参。
///
/// 无 Path extractor：`serial_no` 是主键，`batch_id` 仅在多批次歧义时用于消歧。
///
/// ## `next_process_id`：**仅非顺应工序时必填**
/// 后端先解析批次在工序链上的位置
/// （`shared::batch::chain::resolve_chain_position`，判据与读侧
/// `GET /scan/held` 的 `chain_state` 逐条同源），按下表分流：
///
/// | 链上位置 | 分流 | `next_process_id` |
/// |---|---|---|
/// | `TAIL`（链内最后一道） | **自动送检**：这批做完了，不落生产架 | 可省略 |
/// | `NEXT` 且顺应 | 后端按链推导下一道工序与 step | 可省略 |
/// | 其余（非顺应：无链 / 链已软删 / 指针漂移 / 链内 `process_id` 重复） | 按前端指定推进 | **必填**，缺失 → `40001` |
///
/// 字段类型保持 `Option<String>` 不改 wire：前端可以继续照 `chain_state` 决定
/// 填不填，两条路径都合法。
///
/// ## ⚠️ 响应 `event_type` 可能与请求的**不同**
/// 请求发 `RETURNED` 但批次在链尾时，服务端把它当送检处理，响应
/// `event_type = "WORKER_SCAN_INSPECTED"`。消费方**必须按响应里的 `event_type`
/// 分支**，不能按自己发的那一个 —— 服务端比前端更清楚批次做完了没有。
///
/// ## 两个货架字段已移除（2026-10-10）
/// `shelf_id` 与 `target_inspection_shelf_id` 都由服务端按负载自动选
/// （`shared::shelf::select::pick_least_loaded`）；移除是**向后兼容**的（老客户端
/// 多发的字段被 serde 静默忽略，本仓生产代码零 `deny_unknown_fields`），但新客户端
/// 发老版本服务端会得 422，**部署顺序必须后端先上**。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerScanRequest {
    pub serial_no: String,
    pub badge_code: String,
    pub event_type: WorkerScanEvent,
    #[serde(default)]
    pub next_process_id: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
    /// 本次实际操作数量；缺省 = 整批。
    ///
    /// 2026-10-11 新增。线上形态是 **JSON 字符串**（`deserialize_i64_opt` 只吃
    /// `str`），例如 `"quantity": "4"`；**发数字 → axum `Json` 提取器反序列化失败 →
    /// HTTP 422 纯文本**，不进 `R<T>` 信封。字符串形态照同域 `PickUpRequest::quantity`
    /// —— 报工台一次操作里两个「部分数量」入参保持同形，前端不必记两套写法。
    ///
    /// 三种落法（范围校验在 service 层，错误码 `20111`
    /// `BIZ_PART_BATCH_INVALID_QUANTITY`）：
    /// - `None` / `>= batch.quantity` → **整批**，行为与本字段引入前逐字一致
    ///   （`==` 是「显式整批」的合法写法，语义与 `None` 等价）；
    /// - `0 < quantity < batch.quantity` → 自动拆批：拆出来的那一批走本次流转
    ///   （放回 / 送检 / 链尾自动送检），**余量继承 `current_holder_id` 留在工人
    ///   手上**，继续出现在报工台「已持有」列表里；
    /// - `<= 0` / `> batch.quantity` → `20111`。
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub quantity: Option<i64>,
}

/// `POST /api/v2/prod/scan/batches/{batch_id}/pick-up` 入参（手动 pick-up 兜底）。
///
/// PENDING / IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER。
/// `worker_id` 必填（持有件工人）；`shelf_id` **可选**（见该字段 doc）。
///
/// `quantity` 缺省 = 整批领取（保持既有行为，向后兼容）；
/// `0 < quantity < batch.quantity` 时 service 自动拆批，把拆出来的那部分交给
/// 工人，源批次留在原处、数量递减。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PickUpRequest {
    pub version: i32,
    #[serde(deserialize_with = "deserialize_i64")]
    pub worker_id: i64,
    /// 当前批次所在货架。**可选**：传了才走 `validate_shelf_zone`
    /// （`20501` / `20512` / `20104`），缺省则不校验、不推导、不回退。
    ///
    /// ## 为什么可以缺省：这个字段对最终结果零影响
    ///
    /// pick-up 路径上 `shelf_id` 只进 `validate_shelf_zone`，而它内部只
    /// `SELECT ... FROM t_shelf WHERE id = $1 AND deleted_at IS NULL`（零写）；
    /// 本路径 `t_part_batch` 的全部写入点（拆成 4 条 SQL；`pickup.rs` 内联 SQL、
    /// `guards.rs` → `status.rs` 的通用 UPDATE、部分领取的
    /// `split_batch_for_partial_pass` = `_split_batch_inner` 的 INSERT + UPDATE）
    /// 的 SET 与 WHERE 均无货架列或货架条件；`t_part_event` 无货架列；响应 VO
    /// `PartOut` 无 shelf 字段。
    /// ⇒ 那条校验是**防呆断言**（让手填错区的人当场看见 20104），不是安全边界，
    /// 故不必强绑在成功路径上 —— 扫码台 / 看板等自动发起 pick-up 的调用方本就
    /// 无从知道「批次此刻名义上在哪一个架」。
    ///
    /// ## 为什么不做「从 `current_holder_id` 推导」
    ///
    /// 技术上不可行：
    /// 1. PENDING 起点的批次 `current_holder_id` 恒为 `NULL`（`create_initial_batch`
    ///    写死 `NULL, NULL`），而 PENDING 正是「待下发池」的 pick-up 起点；
    /// 2. IN_PROCESS 起点只守 `location='PRODUCTION_SHELF'`、**不守 holder**，
    ///    `dispatch` 与 `queue/move` 两个写点能把 INSPECTION 区的架写进
    ///    `current_holder_id`；
    /// 3. `current_holder_id` 可能指向**已软删 / 非 PRODUCTION 区**的架，
    ///    「推导 + 施加同样校验」会把这类批次**永久锁死**。
    ///
    /// ## ⚠️ 本字段**无 scope 校验**
    ///
    /// 与 worker-scan 对照：后者对 `shelf_id` 走 `can_access_shelf` 并在越权时
    /// 返 `40301 SHELF_MISMATCH`。pick-up 不做该校验，故本字段缺省时**没有**
    /// 任何货架维度的权限收敛；`require_any_role([Manager, Clerk, ShelfAccount])`
    /// 是本端点唯一的权限边界。
    ///
    /// 线上形态仍是 **JSON 字符串**（`deserialize_i64_opt` 只吃 `str`），
    /// 例如 `"shelf_id": "43"`；`"shelf_id": 43`（数字）→ `422` 纯文本。
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub shelf_id: Option<i64>,
    /// 部分领取数量；缺省 = 整批。
    ///
    /// 线上形态是 **JSON 字符串**（`deserialize_i64_opt` 只吃 `str`），例如
    /// `"quantity": "4"`；发数字 → 422 纯文本。本字段的字符串形态是**前端既有约定**
    /// （扫码台照此发送），与本模块其它「裸 JSON 数字」字段刻意不同形，改它要
    /// 连带改前端。
    ///
    /// `None` / `>= batch.quantity` 一律按整批处理（`==` 是「显式整批」的合法
    /// 写法，语义与 `None` 等价）；`0 < q < batch.quantity` 触发自动拆批。
    /// 范围校验在 service 层（`try_into` 到 `i32` + 上下界），错误码
    /// `BIZ_PART_BATCH_INVALID_QUANTITY`。
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub quantity: Option<i64>,
    /// 事件日志备注。
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/prod/scan/take-over` 入参（扫码接管，2026-10-15 新增 S2）。
///
/// 场景：批次仍挂在**别的**工人名下（`IN_PROCESS + location='WORKER'` +
/// `current_holder_id != 我`），但图纸 / 实物已经流转到这位工人手里 ⇒ 扫工牌一键
/// 接管：holder 换成我、顺手推进到我工种下的下一道工序，同一事务原子完成。
///
/// ## 与 worker-scan 的入参形态关系
/// `batch_id` / `next_process_id` 两个键在两条端点上**同形**（都是雪花 id 的
/// **JSON 字符串**）：
/// - `batch_id` 走 `deserialize_i64`（只吃 `str`；发数字 → axum `Json` 提取器
///   **HTTP 422 纯文本**，不进 `R<T>` 信封）；
/// - `next_process_id` 保持 `Option<String>` 不改 wire（与 `WorkerScanRequest`
///   逐字同款），service 内 `str::parse`，非法值 → `40001 VALIDATION_ERROR`。
///   它**仅在链位置推导不出下一道时必填**（详见 service 的判据顺序）。
///
/// `version` 是**批次 OCC 锚**且**必填**（无 `#[serde(default)]`）：缺失 → 422
/// 纯文本。`badge_code` 是接管人（不是旧持有人）—— 旧持有人由 `batch_id` 行上的
/// `current_holder_id` 决定，客户端不需要、也不应该指定它。
#[derive(Debug, Clone, Deserialize)]
pub struct TakeOverRequest {
    /// 批次锚（雪花 id 的 JSON 字符串）。
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    /// 接管人的工牌码（`t_worker.badge_code`）；反查不到 → `20201`。
    pub badge_code: String,
    /// 批次 OCC 锚（客户端传值；与 `t_part_batch.version` 不一致 → `40901`）。
    pub version: i32,
    /// 接管后要推进到的工序；**仅链位置推导不出下一道时必填**（缺失 → `40001`，
    /// 有链但链内定位不到 → `20702`）。JSON 字符串形态（与 worker-scan 同款）。
    #[serde(default)]
    pub next_process_id: Option<String>,
    /// `t_part_event.note` 的附加备注。缺省时事件备注自动写
    /// `"接管自 worker {旧持有人 id}"`；有值时拼成 `"接管自 worker {id}：{note}"`。
    #[serde(default)]
    pub note: Option<String>,
}
