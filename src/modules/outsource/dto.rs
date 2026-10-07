//! outsource 域 DTO（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/schema/outsource.py。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxOut`：单条详情出参 —— 2026-09-22 PR4 重构移至 `super::vo`
//! - `XxxListItem` / `XxxListOut`：列表分页 —— 2026-09-22 PR4 重构移至 `super::vo`
//! - `XxxListQuery`：列表查询参数
//!
//! ## 与 `super::vo` 的边界
//! 本文件仅保留 `Deserialize` 入参；出参类型（`Out` / `ListOut`）已抽离到
//! `super::vo`，handler 入口需改 `use crate::modules::outsource::vo::*`。
//!
//! ## 2026-10-09：移动写端点入参（`OutsourceLocation` / `OutsourceMoveRequest`）
//! 见文件末「`POST /outsource-queue/move` 入参」一节。

use chrono::NaiveDateTime;
use serde::Deserialize;

use crate::shared::types::{deserialize_i64, deserialize_i64_opt};

// ===========================================================================
// 入参 — Company
// ===========================================================================

/// 创建外协公司（可选一并写入工序能力清单）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceCompanyCreateRequest {
    pub name: String,
    #[serde(default)]
    pub contact_name: Option<String>,
    #[serde(default)]
    pub contact_phone: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default = "default_is_active")]
    pub is_active: bool,
    /// 可选：创建时一并写入工序能力清单（OUTSOURCE 类别的 process_id 列表）。
    #[serde(default)]
    pub process_ids: Option<Vec<String>>,
}

fn default_is_active() -> bool {
    true
}

/// 更新外协公司（字段可选 + 显式 OCC）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceCompanyUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub contact_name: Option<Option<String>>,
    #[serde(default)]
    pub contact_phone: Option<Option<String>>,
    #[serde(default)]
    pub address: Option<Option<String>>,
    #[serde(default)]
    pub is_active: Option<bool>,
    pub version: i32,
}

/// 整体替换工序能力清单。
#[derive(Debug, Clone, Deserialize)]
pub struct SetOutsourceCompanyProcessRequest {
    pub process_ids: Vec<String>,
}

/// 公司列表查询参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceCompanyListQuery {
    #[serde(default)]
    pub name_like: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ===========================================================================
// 入参 — Quote
// ===========================================================================

/// 创建 DRAFT 报价。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteCreateRequest {
    pub part_id: String,
    pub outsource_company_id: String,
    pub process_id: String,
    pub price: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// 更新 DRAFT 报价。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteUpdateRequest {
    #[serde(default)]
    pub price: Option<String>,
    #[serde(default)]
    pub note: Option<Option<String>>,
    pub version: i32,
}

/// 审批通过（MANAGER-only）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteApproveRequest {
    #[serde(default)]
    pub review_note: Option<String>,
    pub version: i32,
}

/// 审批拒绝（MANAGER-only）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteRejectRequest {
    pub review_note: String,
    pub version: i32,
}

/// 报价列表查询参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceQuoteListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub part_id: Option<String>,
    #[serde(default)]
    pub outsource_company_id: Option<String>,
    /// 客户子树过滤（2026-10-04 改语义，此前该字段在 DTO 里存在但被丢弃 ⇒ 恒不过滤）。
    /// service 层展开成 part_id 集合（`part_ids_by_customer` = 自身 ∪ 直接子客户），
    /// 与 `keyword` 展开出的集合**取交集**。
    ///
    /// 展开只下潜一层 —— 依据是 2026-10-04 生产库实测结论（零件全挂 L2、L3 数量 0），
    /// 而**该结构 API 层不强制**（`create_customer` 不校验 `parent_id` 是否指向根
    /// 客户）。出现 L3 后本字段需改成递归子树展开，且漏报**是静默的**（`total` 偏小、
    /// 不报错）。详见 `repo/mod.rs::part_ids_by_customer` 的注释（那里保留了同一谓词的
    /// 另一份拷贝与「一旦出现 L3 必须改递归 CTE」的登记）。
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// 可建报价的（零件 × OUTSOURCE 工序）组合列表查询参数。
///
/// 2026-10-03 新增：前端报价一览页 + 「新建报价」零件 picker 的读侧契约
/// （此前路由未注册，请求被 `/{id}`（`Path<i64>`）吞掉恒返 400）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceQuotablePartListQuery {
    /// drawing_no / name ILIKE 模糊匹配（与 `part_keyword_search` 同语义）。
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ===========================================================================
// 入参 — Shipment
// ===========================================================================

/// 对账页更新 shipment。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceShipmentReconcileUpdateRequest {
    #[serde(default)]
    pub unit_price: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub is_billed: Option<bool>,
    pub version: i32,
}

/// 外协对账页：某公司已发出零件列表查询参数。
///
/// 2026-10-03 新增：`GET /outsource-companies/{id}/sent-parts` 读侧契约
/// （此前路由未注册，前端「外协对账」页 404）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceSentPartListQuery {
    /// part 的 drawing_no / name ILIKE 模糊匹配（复用 `part_keyword_search` 语义）。
    #[serde(default)]
    pub keyword: Option<String>,
    /// `sent_at` 闭区间下界（含）。
    #[serde(default)]
    pub sent_from: Option<NaiveDateTime>,
    /// `sent_at` 闭区间上界（含）。
    #[serde(default)]
    pub sent_to: Option<NaiveDateTime>,
    /// `received_at` 闭区间下界（含）。
    #[serde(default)]
    pub received_from: Option<NaiveDateTime>,
    /// `received_at` 闭区间上界（含）。
    #[serde(default)]
    pub received_to: Option<NaiveDateTime>,
    /// 排序列白名单：`PRICE` / `SENT_AT` / `RECEIVED_AT`；非法值回落 `SENT_AT`。
    /// **绝不把本字段拼进 SQL** —— service 只归一化成白名单 token 后 bind。
    #[serde(default)]
    pub sort_by: Option<String>,
    /// `ASC` / `DESC`；非法值回落 `DESC`。
    #[serde(default)]
    pub sort_dir: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// 外协在途批次列表查询参数。
///
/// 2026-10-03 新增：`GET /outsource-shipments/in-flight` 读侧契约
/// （替代 part 域错形状的 `/parts/outsource-in-flight`）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceInFlightListQuery {
    /// part 的 drawing_no / name ILIKE 模糊匹配。
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ===========================================================================
// 入参 — Sendable（`GET /outsource-sendable` 已于 2026-10-09 硬切下线，
//   候选列内联进 `GET /outsource-queue/processes/{id}` 的看板左列；
//   分页 + 关键字 + 客户过滤那套入参随之删除）
// ===========================================================================

// ===========================================================================
// 入参 — Move（`POST /api/v2/outsource-queue/move`）
// ===========================================================================

/// 外协看板上的一个位置（`from` / `to` 共用同一形状）。
///
/// ## `kind` 取值必须与 `t_part_batch.location` 的枚举值**逐字对齐**
///
/// `PRODUCTION_SHELF` / `OUTSOURCE_COMPANY` / `INSPECTION_SHELF` 三值取自该列的
/// 域内取值（`OFFICE` / `WORKER` 两个值不进外协看板：一个是工单建档位的非批次态、
/// 一个是厂内工人持有位）。**这条对齐是整个三合一设计成立的前提**：
///
/// - 不变式 1：`from.kind` **必须等于**批次当前 `location`，否则 service 以
///   `20122 BIZ_BATCH_LOCATION_MISMATCH` 拒收；
/// - 不变式 2：`new_location` 直接取 `to.kind` 写入 `t_part_batch.location`。
///
/// 两者合起来意味着 **`from` 就是移动的乐观并发锚**（照 `prod::queue` 的
/// `MoveLocation`）：前端不必（也不应）另传一个「我以为批次在哪」的字段，一旦
/// 有人改动这组字面量与 DB 取值的关系，破坏的不是某个端点而是「from 守卫」本身 ——
/// 批次会被静默搬到错误的位置而没有任何守卫拦它。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutsourceLocation {
    /// 生产货架（候选池）。作为 `to`（回收）时 `next_process_id` 语义见
    /// [`OutsourceMoveRequest`]；作为 `from` 时留空（`shelf_id` 必须等于批次真实
    /// `current_holder_id`）。
    ProductionShelf {
        #[serde(deserialize_with = "deserialize_i64")]
        shelf_id: i64,
        /// **仅 `to` 需要**：回收后进入的下一道 INHOUSE 工序。
        ///
        /// 缺省时后端从工序链推导（`next_process` 相关口径见
        /// [`OutsourceMoveRequest`] 与 `service/move.rs` 的推导 SQL），推不出则
        /// `20706 BIZ_PROCESS_CHAIN_REQUIRED`。作为 `from` 时必须留空 —— 源货架的
        /// 「下一道工序」对本次移动无意义，填了反而会让调用方误以为它参与校验
        /// （实际上写侧不读它）。
        #[serde(default, deserialize_with = "deserialize_i64_opt")]
        next_process_id: Option<i64>,
    },
    /// 外协公司（在途中）。批次收回时它的 id 就是新 `current_holder_id`。
    OutsourceCompany {
        #[serde(deserialize_with = "deserialize_i64")]
        company_id: i64,
    },
    /// 品检货架（回收直送品检）。只有 `to` 方向出现（`from` 不可能是品检架上的
    /// 在外协批次）。
    InspectionShelf {
        #[serde(deserialize_with = "deserialize_i64")]
        shelf_id: i64,
    },
}

/// `POST /api/v2/outsource-queue/move` 入参。
///
/// 三合一写端点，取代三个单边端点（`POST /prod/batches/{batch_id}/send-to-outsource`
/// / `receive-from-outsource` / `receive-from-outsource-to-inspection`，**硬切无
/// alias**）。支持的三个方向：
///
/// | `from` | `to` | 状态迁移 |
/// |---|---|---|
/// | `PRODUCTION_SHELF` | `OUTSOURCE_COMPANY` | `PENDING` / `IN_PROCESS` → `OUTSOURCE` |
/// | `OUTSOURCE_COMPANY` | `PRODUCTION_SHELF` | `OUTSOURCE` → `IN_PROCESS`（推进到 `next_process_id`） |
/// | `OUTSOURCE_COMPANY` | `INSPECTION_SHELF` | `OUTSOURCE` → `INSPECTION`（`current_process_id` 置 NULL） |
///
/// ## 为什么不带 `quantity`（整批语义）
/// 旧三端点带 `quantity` 支持部分收发（写侧先拆批、只流转子批次）。看板卡片是
/// **批次卡**（行 = 批次），拖拽语义就是「把这一批挪过去」，部分流转走独立拆批端点
/// `POST /prod/batches/{batch_id}/split`。混进来会让「一个 move 写两行批次」的记账
/// 与 shipment 的开口 / 关闭口径在三合一端点里分叉（shipment 记的是**发出时的
/// 全量**，部分回收只拆批次、源批次继续持有开口 shipment）—— 那是三合一要消灭的
/// 分歧。
///
/// ## 为什么不带 `process_id`（后端自推）
/// 外协加工的工序就是批次**当前所属工序**（`t_part_batch.current_process_id`）。
/// 让调用方传它等于多开一个与批次状态不一致的自由度，而读侧候选卡本就是按该列
/// 选行的。
///
/// ## 一个必须知道的形态收窄：发送的起点恒是「在架上的批次」
/// `from.kind` 只有三个取值，其中作为发送起点的是 `PRODUCTION_SHELF` —— 因此
/// **`location IS NULL` 的 `PENDING` 批次（还没上架）不再能直接发外协**，要先走
/// `POST /api/v2/prod/batches/{batch_id}/place-on-shelf`。这与看板候选卡的形态一致：
/// 那类行的 `shelf_id` 序列化成空串（VO 的 doc 逐字写了「传空串给 `from.shelf_id`
/// 会被写端点拒收」），旧单边端点允许直接从 `PENDING` 发是绕过看板的旁路。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceMoveRequest {
    /// 被移动的批次（`t_part_batch.id`）。
    ///
    /// 2026-10-09 **照 `prod::queue::dto::MoveRequest` 的同款形态放在 body**（旧三个
    /// 单边端点把它放在 URL path 里，三合一后路径退化成静态段 `/move`，主键只能进
    /// body —— 本域其余写端点也都是 ID 全走 body）。
    #[serde(deserialize_with = "deserialize_i64")]
    pub batch_id: i64,
    /// 批次级 OCC 锚（`t_part_batch.version`）：读侧候选卡 / 在途卡的 `version`
    /// 原样回传，漏传或过期返 `40901`。
    ///
    /// **无 `#[serde(default)]`**，故缺省是 axum 的 `422` + 纯文本
    /// `missing field \`version\``（不是业务信封）。
    pub version: i32,
    /// 批次当前所在位置。必须与批次真实 `(location, current_holder_id)` 一致，
    /// 否则 `20122`。
    pub from: OutsourceLocation,
    /// 目标位置。
    pub to: OutsourceLocation,
    /// **仅发送方向**（`to.kind = OUTSOURCE_COMPANY`）：APPROVAL 模式传**已批准
    /// 报价**的 id。
    ///
    /// DIRECT 方向必须传 `null`：写侧从 `(part, company, process)` 找活跃 APPROVED
    /// 报价复用、找不到就自动建 `price=0` 的占位报价，由 `direct=true` 表达这个意图
    /// （见 `service/move.rs::resolve_direct_quote_id`）。
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub quote_id: Option<i64>,
    /// **仅发送方向**：DIRECT（免审批直发）传 `true`；APPROVAL 传 `null`
    /// （`false` 与 `null` 同义 —— 写侧取 `unwrap_or(false)`，两者都走 APPROVAL
    /// 分支，但契约上以 `null` 为准，避免调用方把「不需要审批」与「审批了但没给
    /// 报价 id」两种语义混着传）。
    #[serde(default)]
    pub direct: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
}
