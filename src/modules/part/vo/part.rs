//! part 域单件详情 / 列表 / 事件出参 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
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

/// `GET /parts/pending-programming` 出参：PROGRAMMING 状态工单一览（复用 PartListOut）。
pub type PendingProgrammingOut = PartListOut;