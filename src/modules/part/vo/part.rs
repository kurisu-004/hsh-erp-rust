//! part 域单件详情 / 列表 / 事件出参 VO（2026-09-22 PR4 重构）
//!
//! 2026-09-27 review 第 1 轮修复：`PartListItem` 改显式列字段（不再 flatten
//! `TPart`），列表响应不再包含 `next_process_id`，但 `PartDetailOut` 仍 flatten
//! `TPart` —— 详情响应仍含 `next_process_id`。两端语义保持：
//! - list：不返 next_process_id
//! - detail：仍返 next_process_id

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::modules::part::model::{TPart, TPartInspected};
use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// 工单详情投影（to-ship / to-inspection / to-process 出参；其它端点复用做最小投影）。
///
/// 字段集与 `model::TPartInspected` 完全对齐：仅含 to-XXX 流程与最小
/// `PartOut` 响应必需列。完整业务字段（`applicant_name` / `unit_price` 等）待
/// part 域业务实施时再补全。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `actual_delivery_date` 字段
/// （t_part 列已删；实际交付日期由 t_part_event DELIVERED 事件派生，前端
/// 按需额外调 statistics 端点获取）。
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
    pub updated_at: NaiveDateTime,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub updated_by: Option<i64>,
}

impl From<TPartInspected> for PartOut {
    fn from(p: TPartInspected) -> Self {
        Self {
            id: p.id,
            serial_no: p.serial_no,
            name: p.name,
            drawing_no: p.drawing_no,
            status: p.status,
            version: p.version,
            quantity: p.quantity,
            order_no: p.order_no,
            updated_at: p.updated_at,
            updated_by: p.updated_by,
        }
    }
}

/// 从完整 `TPart` 投影到 `PartOut`。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `actual_delivery_date` 字段；
/// `delivery_note_id` 守卫改在 service 层用
/// `PartBatchRepo::has_active_batch_on_delivery_note` 预检（不再依赖
/// TPart.delivery_note_id 字段）。
impl From<TPart> for PartOut {
    fn from(p: TPart) -> Self {
        Self {
            id: p.id,
            serial_no: p.serial_no,
            name: p.name,
            drawing_no: p.drawing_no,
            status: p.status,
            version: p.version,
            quantity: p.quantity,
            order_no: p.order_no,
            updated_at: p.updated_at,
            updated_by: p.updated_by,
        }
    }
}

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

/// `GET /parts` 列表行：`TPart` 显式列字段 + 客户冗余字段 + 派生位置 / 持有人。
///
/// 2026-09-27 review 第 1 轮修复：放弃 `#[serde(flatten)] pub part: TPart`，
/// 改为显式列字段（22 字段 + 4 派生字段）。目的是「**列表不返 next_process_id** +
/// **详情仍返 next_process_id**」（`PartDetailOut` 仍 flatten `TPart`，保持响应）。
///
/// 字段集对齐 `TPart` 25 列去掉 `next_process_id`（24 列）：含 2026-09-27
/// 新增 `unit_price` / `total_price` NUMERIC 金额列、`process_chain_id`
/// 逻辑 FK、2026-09-16 PR-2 瘦身后的 23 列全集。
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
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub applicant_name: String,
    pub quantity: i32,
    pub request_date: NaiveDate,
    pub planned_delivery_date: NaiveDate,
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub assembly_id: Option<i64>,
    pub status: String,
    pub is_urgent: bool,
    // 注意：next_process_id 不在 list 响应里（2026-09-27 用户决策范围 C）
    pub order_no: Option<String>,
    pub system_delivery_date: Option<NaiveDate>,
    pub note: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    pub unit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_price: Decimal,
    pub version: i32,
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub process_chain_id: Option<i64>,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
    /// 派生位置（见字段级 doc 注释）。
    #[serde(default)]
    pub location: Option<String>,
    /// 派生持有人名称（见字段级 doc 注释）。
    #[serde(default)]
    pub holder_name: Option<String>,
    /// 2026-09-28 新增：行类型标识（ALL 模式合并列表用）。
    /// - PART 模式 / ALL 模式零件段 → `Some("PART")`
    /// - ASSEMBLY 模式 / ALL 模式装配件段 → `Some("ASSEMBLY")`
    /// - 历史 caller（不传 `row_type` / `include_assemblies` 时旧 PART-only 行为）→ `None`
    ///   前端按此字段区分渲染（"PART" 走普通行；"ASSEMBLY" 走 Tree 节点 + lazy load）。
    #[serde(default)]
    pub row_type: Option<String>,
    /// 2026-09-28 新增：是否有子件（Tree data lazy mode 必需）。
    /// - PART 行 → `false`（零件不可展开）
    /// - ASSEMBLY 行 → `child_count.unwrap_or(0) > 0`
    ///   仅前端需要据此切换展开/折叠交互；后端不强制走 `GET /assemblies/{id}` 预拉。
    #[serde(default)]
    pub has_children: bool,
    /// 2026-09-28 新增：子件计数（装配体行专用）。PART 行 → `None`。
    /// 与 `has_children` 配套：`has_children = child_count.unwrap_or(0) > 0`。
    /// 真相源：`t_part WHERE assembly_id = $1 AND deleted_at IS NULL` 的 COUNT。
    #[serde(default)]
    pub child_count: Option<i64>,
    /// 2026-09-29 新增：是否已上传 G_CODE（数控程序）。
    /// - 真相源：`EXISTS (SELECT 1 FROM t_part_file WHERE part_id = p.id AND kind = 'G_CODE' AND deleted_at IS NULL)`
    /// - 用途：`GET /parts/pending-programming` 的 Tab 切换 (`has_cnc_program` query 参数) +
    ///   worker_pool 候选池的自动分配优先级（已上传 G_CODE 的批次优先 take）
    /// - 其它列表端点（`GET /parts`）默认 `false`（service 层不再 enrich；前端如需，
    ///   走 `GET /parts/pending-programming` 即可拿到此字段）
    #[serde(default)]
    pub has_cnc_program: bool,
    /// 2026-10-03 新增：活跃批次雪花 id（序列化走 `serialize_i64_opt` → JSON string）。
    ///
    /// **仅 `GET /parts/pickable-by-work-type/{work_type_id}` 填** —— 该端点的行
    /// 本来就是「批次行」（取行 SQL 从 `t_part_batch b` 起），扫码台「领料」按本
    /// 字段定位批次后发写请求。
    ///
    /// 其余复用 `PartListItem` 的路径（`GET /parts` / `GET /com/union-list` /
    /// `GET /parts/pending-programming` 等）**恒为 `None`**：那些行的语义单位是
    /// part，一个 part 的活跃批次可能不止一个，填任一活跃批次都是错锚点，故宁可不填。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub batch_id: Option<i64>,
    /// 2026-10-03 新增：`batch_id` 那个批次的乐观锁版本号（`t_part_batch.version`）。
    ///
    /// 前端发写请求时作 OCC 版本回传。⚠️ 本 VO 的 `version` 字段是 **part 级**
    /// （`t_part.version`），与批次 OCC 无关；批次 OCC 只认本字段，**不要拿
    /// `version` 当批次版本用**。填充口径与 `batch_id` 完全一致（同为「仅
    /// pickable-by-work-type 填，其余路径 null」）。
    #[serde(default)]
    pub batch_version: Option<i32>,
}

impl From<TPart> for PartListItem {
    fn from(p: TPart) -> Self {
        Self {
            id: p.id,
            serial_no: p.serial_no,
            name: p.name,
            drawing_no: p.drawing_no,
            applicant_name: p.applicant_name,
            quantity: p.quantity,
            request_date: p.request_date,
            planned_delivery_date: p.planned_delivery_date,
            customer_id: p.customer_id,
            assembly_id: p.assembly_id,
            status: p.status,
            is_urgent: p.is_urgent,
            // 故意不复制 p.next_process_id：列表响应不暴露该字段
            order_no: p.order_no,
            system_delivery_date: p.system_delivery_date,
            note: p.note,
            unit_price: p.unit_price,
            total_price: p.total_price,
            version: p.version,
            created_at: p.created_at,
            created_by: p.created_by,
            updated_at: p.updated_at,
            updated_by: p.updated_by,
            deleted_at: p.deleted_at,
            process_chain_id: p.process_chain_id,
            customer_name: None,    // 由 service 注入
            l1_customer_name: None, // 由 service 注入
            location: None,         // 由 service 注入
            holder_name: None,      // 由 service 注入
            // 2026-09-28 新增：TPart 派生默认就是 PART 行
            row_type: Some("PART".to_string()),
            has_children: false,
            child_count: None,
            // 2026-09-29 新增：默认 false；`From<TPart>` 不做 EXISTS enrich
            // （避免 N+1）；service 层在 `list_pending_programming` / `list_parts`
            // 调用 repo 时通过 EXISTS 子查询填充。其它 list caller 不填 → 默认 false
            // （前端无影响，因为这些 caller 不暴露此字段语义）。
            has_cnc_program: false,
            // 2026-10-03 新增：`From<TPart>` 是 part 级投影，不含批次语义
            // （`TPart` 本身不持 batch_id）→ 恒 None。需要批次锚点的端点
            // （目前只有 `pickable-by-work-type`）在 `PartListItem::from` 之后
            // 显式覆写这两个字段。
            batch_id: None,
            batch_version: None,
        }
    }
}

/// `GET /parts` 出参（分页）。
///
/// 2026-09-27 part 域前后端字段对齐：`total` / `limit` / `offset` 改裸 i64 →
/// JSON number，对齐其它 9 域（UserListOut / CustomerListOut / WorkerListOut /
/// OutsourceCompanyListOut / ProcessListOut / ShelfListOut / DeliveryNoteListOut /
/// DeliveryGroupListOut / OutsourceQuoteListOut）。雪花 ID 仍走 `serialize_i64`
/// → JSON string 规避 JS `Number.MAX_SAFE_INTEGER` 精度截断，本处分页字段是
/// 普通 i64，无精度风险，直接 JSON number。
#[derive(Debug, Clone, Serialize)]
pub struct PartListOut {
    pub items: Vec<PartListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
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
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub created_by: Option<i64>,
}

/// `GET /parts/{id}/batches` 出参：工单全部活跃批次 + holder 名称解析。
///
/// 2026-09-30 Phase 2 dashboard 二次调整（fix Zod 报错）：
/// - 新增 7 字段：`batch_label`（mapper 内 `format!("L{}", id)` 派生）/ `part_id` /
///   `current_holder_display`（重命名原 `holder_name`）/ `current_process_step_id` /
///   `next_process_name`（LEFT JOIN t_process 派生）/ `delivery_note_no`（LEFT JOIN
///   t_delivery_note 派生）/ `created_at` / `updated_at`
/// - 修复前端 dashboard PartPreviewDialog 运行时 Zod 校验报错（`received undefined`
///   for these fields, since frontend schema declares them but backend wire 不投）
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
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64, // 2026-09-30 新增（来自 b.part_id）
    pub batch_no: i32,
    /// 2026-09-30 新增（沿 delivery_note L{id} 命名约定，mapper 内 format! 生成）
    pub batch_label: String,
    pub quantity: i32,
    pub status: String,
    /// 2026-10-01 review 第 1 轮 M5 新增（migration 005）：`REPAIRING` 已从
    /// `PartStatus` 降级为 `t_part_batch.is_repairing` 标记列，故返修中的批次
    /// `status` 恒为 `IN_PROCESS` —— 「是否返修中」只能由本字段表达。
    pub is_repairing: bool,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_holder_id: Option<i64>,
    /// 2026-09-30 重命名（原 `holder_name`）—— 前端 usePartDetail.ts:441 已读
    /// `current_holder_display`，rename 为正本。
    #[serde(default)]
    pub current_holder_display: Option<String>,
    /// 2026-09-30 新增（2026-09-16 PR-3 后 SQL 已选 b.current_process_step_id 但 DTO 未投）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_process_step_id: Option<i64>,
    /// 2026-09-16 PR-3：DTO 保留字段名（兼容前端），但当前**全部为 None**——
    /// 业务上「下一步工序」概念已迁移到 step（current_process_step_id →
    /// JOIN step.process_id 派生）；新端点不应依赖该字段。如前端仍需该信息，
    /// 由 frontend 自行 JOIN current_process_step_id → step.process_id。
    ///
    /// 2026-09-30 Phase 2 调整：mapper 内填 next_process_id 同源值（LEFT JOIN
    /// t_process_chain_step.process_id 派生）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    /// 2026-09-30 新增（LEFT JOIN t_process 派生）
    #[serde(default)]
    pub next_process_name: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub delivery_note_id: Option<i64>,
    /// 2026-09-30 新增（LEFT JOIN t_delivery_note 派生）
    #[serde(default)]
    pub delivery_note_no: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub parent_batch_id: Option<i64>,
    /// 2026-09-30 新增（t_part_batch.created_at NOT NULL TIMESTAMP）
    pub created_at: NaiveDateTime,
    /// 2026-09-30 新增（t_part_batch.updated_at NOT NULL TIMESTAMP）
    pub updated_at: NaiveDateTime,
    pub version: i32,
}

/// `GET /parts/pending-programming` 出参：PROGRAMMING 状态工单一览（复用 PartListOut）。
///
/// 2026-09-29 改造：基于 `t_process.is_cnc` 列的新过滤规则（链上含 CNC step 或
/// 当前批次位于 CNC 货架）+ 新增 `has_cnc_program` 字段（按 `t_part_file.kind='G_CODE'`
/// 派生）+ 新 query 参数 `has_cnc_program?: bool`（Tab 切换）。详见
/// [`PartListItem`](Self#structfield.has_cnc_program)。
pub type PendingProgrammingOut = PartListOut;
