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
//!
//! ## 2026-10-09：公司 / 报价两域收敛
//! - `POST /{id}/processes` 硬切下线，其入参类型删除、功能吸收进
//!   [`OutsourceCompanyUpdateRequest::process_ids`]（三态可分）；
//! - `POST /outsource-quotes/{id}/update` 硬切下线（前端零消费），入参类型随之删除；
//! - `POST /{id}/soft-delete`（公司 / 报价）与 `POST /{id}/submit`（报价）补必填
//!   `version` OCC 锚 —— 这三条此前都是 service 内部自读 version，等于用自己读到的
//!   值守自己的乐观锁。
//! - 两个列表端点的 `keyword` 拆成 `drawing_no` + `name` 直连 ILIKE（理由见各自
//!   字段注释：旧的 `part_keyword_search` 预搜索有 `LIMIT 10000` 无 `ORDER BY` 的
//!   静默截断风险）。

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
///
/// 2026-10-09：`process_ids` 从独立的 `POST /{id}/processes` 端点吸收进来（该端点
/// 本轮硬切下线，无 alias）。吸收的副作用是「改个电话号码」也会走到工序映射的写入
/// 路径，故 service 侧加了一道「目标集合 == 当前集合就跳过重写」的守卫。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceCompanyUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// 三态：`None` 不改 / `Some(None)` 置 null / `Some(Some(s))` 赋值。
    #[serde(default)]
    pub contact_name: Option<Option<String>>,
    #[serde(default)]
    pub contact_phone: Option<Option<String>>,
    #[serde(default)]
    pub address: Option<Option<String>>,
    #[serde(default)]
    pub is_active: Option<bool>,
    pub version: i32,
    /// 整体替换工序能力清单。三态可分（`Option<Vec<_>>` + `#[serde(default)]` 天然
    /// 能分）：缺省 / `null` = 不动工序映射、`[]` = 清空、`[..]` = 替换。
    ///
    /// 「清空」必须能与「不动」区分，否则「取消全选并保存」会被当成没改而静默丢失 ——
    /// 合并成一个对话框后，工序勾选框与联系人字段是同一次保存。
    #[serde(default)]
    pub process_ids: Option<Vec<String>>,
}

/// `POST /outsource-companies/{id}/soft-delete` 入参。
///
/// 2026-10-09 加必填 `version`：软删此前是本域唯一没有 OCC 锚的公司写端点，
/// 写侧「先查工序映射非空 → 21205，再让 UPDATE 自读 version 守」意味着用户在弹窗
/// 打开期间改了这家公司的工序，收到的仍是「仍映射 N 项工序」这条与真实原因无关的
/// 提示。现在 version 由调用方传，且**先守 version 再查映射**（理由见
/// `service/company.rs::soft_delete_company` 的注释）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceCompanySoftDeleteRequest {
    /// 无 `#[serde(default)]`：缺省是 axum 的 `422` + 纯文本 `missing field`。
    pub version: i32,
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

/// `POST /outsource-quotes/{id}/submit` 入参。
///
/// 2026-10-09 加必填 `version`：本端点此前无 body，service 在函数内部
/// `quote_get_by_id` 读到当前 version 再喂给 `quote_submit` —— 等于「用服务端自己
/// 读到的值守自己的乐观锁」，`UPDATE` 恒命中 0 行也不会被察觉，守卫形同虚设。
/// approve / reject 早就是显式传 version，submit 是最后一个漏网的。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteSubmitRequest {
    /// 无 `#[serde(default)]`：缺省是 axum 的 `422` + 纯文本 `missing field`。
    pub version: i32,
}

/// `POST /outsource-quotes/{id}/soft-delete` 入参。
///
/// 2026-10-09 加必填 `version`，理由同 [`OutsourceQuoteSubmitRequest`]（原实现是
/// `quote_soft_delete(id, q.version, …)`，拿自己读到的 version 当守卫）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourceQuoteSoftDeleteRequest {
    pub version: i32,
}

/// 报价列表查询参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceQuoteListQuery {
    #[serde(default)]
    pub status: Option<String>,
    /// 多状态筛选，**逗号分隔单值**：`?statuses=DRAFT&statuses=SUBMITTED` 或
    /// `?statuses=DRAFT,SUBMITTED`（等价）。
    ///
    /// 2026-10-09 接线。这是本域最隐蔽的一个 bug 的修复：SQL 与 repo 两层早就支持
    /// （`quote_list_with_filters` 的 `statuses: &[String]` 形参 + WHERE 里的
    /// `AND (cardinality($2::text[]) = 0 OR status = ANY($2))`），只有 DTO 少字段、
    /// service 恒传 `&[]`。症状是静默的：前端一直发状态筛选参数，而 DTO 没有对应字段
    /// ⇒ serde **忽略未知 query 参数**（不报错）⇒ 状态筛选恒不生效 —— 连前端的角色
    /// 默认筛选（MANAGER → `['SUBMITTED']`、CLERK → `['DRAFT']`）也没生效，MANAGER
    /// 打开报价一览看到的是全量报价，且表头因 `statusFilterActive` 变蓝加粗、视觉上在
    /// 说「筛选已生效」。
    ///
    /// ## 为什么是「逗号分隔的 `Option<String>`」而不是 `Option<Vec<String>>`
    /// axum 的 `Query` 走 `serde_urlencoded`，而它的 `Part` 反序列化器**不支持序列**：
    /// `Vec<String>` 字段只有 `#[serde(default)]` 一条出路 —— 重复 key
    /// （`?statuses=A&statuses=B`）、CSV（`?statuses=A,B`）、括号（`?statuses[]=A`）
    /// 三种写法**要么反序列化报错（400 `VALIDATION_ERROR`，纯文本）、要么字段恒为
    /// `None`**，没有一种能真的填出值。已实测（`serde_urlencoded 0.7`）：
    /// `?statuses=A&statuses=B` → `Err("invalid type: string \"A\", expected a sequence")`；
    /// `?statuses[]=A&statuses[]=B` → 键名带 `[]` 不匹配字段名 → `statuses: None`。
    /// 全仓其它多状态 query 因此一律是逗号分隔的 `Option<String>`（`prod::part` 的
    /// `PartListQuery.statuses`、`delivery_note` 的列表入参），本字段与之对齐，
    /// service 侧 `split(',')` 展开。
    ///
    /// ⚠️ 重复 key 形态（`?statuses=A&statuses=B`）在本字段上**取最后一个**
    /// （`serde_urlencoded` 把 query 解析成 `HashMap<key, String>`，后写覆盖先写），
    /// 不是「OR」。要传多值必须用 CSV。
    #[serde(default)]
    pub statuses: Option<String>,
    #[serde(default)]
    pub part_id: Option<String>,
    #[serde(default)]
    pub outsource_company_id: Option<String>,
    /// 客户子树过滤：service 层展开成 part_id 集合（`part_ids_by_customer` = 自身 ∪
    /// 直接子客户），与 SQL 侧的其它谓词**同时生效**（各占各的 WHERE 段，不是交集
    /// 运算）。
    ///
    /// 展开只下潜一层 —— 依据是 2026-10-04 生产库实测结论（零件全挂 L2、L3 数量 0），
    /// 而**该结构 API 层不强制**（`create_customer` 不校验 `parent_id` 是否指向根
    /// 客户）。出现 L3 后本字段需改成递归子树展开，且漏报**是静默的**（`total` 偏小、
    /// 不报错）。详见 `repo/mod.rs::part_ids_by_customer` 的注释（那里保留了同一谓词的
    /// 另一份拷贝与「一旦出现 L3 必须改递归 CTE」的登记）。
    #[serde(default)]
    pub customer_id: Option<String>,
    /// `t_part.drawing_no` ILIKE 模糊匹配（值在 service 层归一化成 `%kw%` 后 bind）。
    ///
    /// 2026-10-09 新增，取代 `keyword`。旧 `keyword` 走 `part_keyword_search` 预搜索
    /// 再用 `part_id = ANY($N)` 回筛，那条预搜索带 `LIMIT 10000` 且**无 `ORDER BY`**
    /// ⇒ 触顶时静默返回非确定性子集、`total` 偏小；且因为谓词是「空数组即不过滤」，
    /// 还必须靠 service 层一个易漏的「零命中早返回」兜底。直连 ILIKE 把这三件事
    /// （截断风险 / 零命中守卫 / 中间数组）一起消掉。
    #[serde(default)]
    pub drawing_no: Option<String>,
    /// `t_part.name` ILIKE 模糊匹配（归一化同上）。
    #[serde(default)]
    pub name: Option<String>,
    /// `t_part.is_urgent = ?` 精确筛选。
    #[serde(default)]
    pub is_urgent: Option<bool>,
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
///
/// 2026-10-09：`keyword` 拆成 `drawing_no` + `name` 两个直连 ILIKE 的字段，并新增
/// `customer_id` / `process_id` / `is_billed` 三个维度（见各字段注释）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceSentPartListQuery {
    /// `t_part.drawing_no` ILIKE 模糊匹配（service 层归一化成 `%kw%` 后 bind）。
    #[serde(default)]
    pub drawing_no: Option<String>,
    /// `t_part.name` ILIKE 模糊匹配（归一化同上）。
    #[serde(default)]
    pub name: Option<String>,
    /// `t_part.customer_id` **等值**过滤（零件直属客户）。
    ///
    /// ⚠️ 与 `GET /outsource-quotes/` 的 `customer_id` **语义不同**：那边在 service
    /// 层展开成「自身 ∪ 直接子客户」的子树（前端选客户时给的常是 L1 客户），这边
    /// 只判零件的直属 `t_part.customer_id` 等值。理由是本端点的筛选列是零件本身的
    /// 归属客户（对账时按「这家外协厂供过哪个客户的货」筛），不是「客户视角的
    /// 报价归属」；前者与 batch / 工序域的 `customer_id` 谓词同形。
    #[serde(default)]
    pub customer_id: Option<String>,
    /// `t_outsource_shipment.process_id` 等值过滤。
    #[serde(default)]
    pub process_id: Option<String>,
    /// `t_outsource_shipment.is_billed = ?` 精确过滤（已开票 / 未开票分账用）。
    #[serde(default)]
    pub is_billed: Option<bool>,
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
