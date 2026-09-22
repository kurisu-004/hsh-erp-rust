//! delivery_note 域 P3 扫码入单 端点响应 VO
//!
//! 包含 ScanOutcomeDto / BatchStatusDto / ResolvedKind / ResolvedEntity /
//! ScanDeliveryNoteSummaryDto / RecentItemDto / AddedBatchDto / UnresolvedTargetDto /
//! AvailableBatchDto / AttachableBatchDto / ScanDeliveryOut。

use serde::Serialize;

/// 扫码入单结果（200 OK 路径）。
///
/// - `ADDED`：A 组覆盖所有 target，本次成功挂载 ≥1 个
/// - `ALREADY_PRESENT`：A 组覆盖所有 target，但都已在本单（幂等）
/// - `CANDIDATES_AVAILABLE`：散件仅 B 组 → unresolved_targets 单元素
/// - `PARTIAL_ADDED`：装配件 A+B 混合 → unresolved_targets 多元素
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScanOutcomeDto {
    Added,
    AlreadyPresent,
    CandidatesAvailable,
    PartialAdded,
}

/// `t_part_batch.status` 强类型投影。序列化沿用 DB 列值（SCREAMING_SNAKE_CASE）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BatchStatusDto {
    Pending,
    Programming,
    InProcess,
    Inspection,
    ReadyToShip,
    Delivered,
    Repairing,
    Outsource,
    Completed,
    Cancelled,
}

impl BatchStatusDto {
    /// 由 DB 字符串反序列化为枚举；未知值返回 `None`。
    #[allow(clippy::should_implement_trait)]
    pub fn from_db(s: &str) -> Option<Self> {
        Some(match s {
            "PENDING" => Self::Pending,
            "PROGRAMMING" => Self::Programming,
            "IN_PROCESS" => Self::InProcess,
            "INSPECTION" => Self::Inspection,
            "READY_TO_SHIP" => Self::ReadyToShip,
            "DELIVERED" => Self::Delivered,
            "REPAIRING" => Self::Repairing,
            "OUTSOURCE" => Self::Outsource,
            "COMPLETED" => Self::Completed,
            "CANCELLED" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// 解析结果类别（驱动前端"是装配件还是散件"决策）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResolvedKindDto {
    Part,
    Assembly,
}

/// 解析结果（识别出来的实体）。
///
/// - `kind = Part`：单工单（可能隶属于某个装配件的子件）；`id` = part.id
/// - `kind = Assembly`：扫的是装配件总图，`id` = assembly.id
///
/// **路线 B 重构（2026-08-27）移除字段**：
/// - `assembly_id`：scan 路径不会扫到子件；如需查父装配体走 `GET /api/v2/assemblies/{id}`
/// - `child_count`：候选列表响应里前端不需要"装配体有几个子件"；要查子件清单走 `GET /api/v2/assemblies/{id}/children`
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedEntityDto {
    pub kind: ResolvedKindDto,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
}

/// 扫码命中的送货单概要（响应里的 `note` 字段）。
#[derive(Debug, Clone, Serialize)]
pub struct ScanDeliveryNoteSummaryDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub delivery_note_no: String,
    pub version: i32,
    pub status: String,
    pub scope_label: String,
    pub customer_path: String,
    pub line_count: usize,
    /// 最近加入该草稿的批次条目（按 batch id DESC 最多 8 条；空单 = 空 Vec）。
    ///
    /// 2026-08-22 新增：原只有 `line_count` 总数，前端草稿卡片要直接展示
    /// 「最近加入序列号/名称/订单号」又不想 N 次额外 GET，于是 DTO 一次性
    /// 把这些字段塞过来。`order_no` 是 Option（工单可能没填）。
    pub recent_items: Vec<RecentItemDto>,
}

/// 草稿卡片里要展示的最近批次条目。
///
/// 2026-08-22：原 `AddedBatchDto` 没有 drawing_no/name/order_no，前端卡片
/// 需要这些字段直接展示（序列号 + 名称 + 订单号），避免每次 N 次
/// GET /notes/{id}。这里独立成一个 DTO，与 added_batches 用 `AddedBatchDto`
/// （极简）解耦。
#[derive(Debug, Clone, Serialize)]
pub struct RecentItemDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    /// 工单序列号；t_part.serial_no 是 nullable → 落 Some/None。
    /// 序列化永远给字符串（None → null），与 ScanDeliveryNoteSummaryDto 内
    /// 其它字段对齐。
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    /// 工单订单号；nullable → Some/None。
    pub order_no: Option<String>,
}

/// 已挂载批次（`added_batches[]`）；跨子件场景 part_id/serial_no 必填。
#[derive(Debug, Clone, Serialize)]
pub struct AddedBatchDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub serial_no: String,
    pub quantity: i32,
}

/// 未就绪 part + 其 B 组候选批次（`unresolved_targets[]`）。
/// 散件场景：单元素；装配件场景：每个未就绪子件一个元素。
#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedTargetDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
    pub available_batches: Vec<AvailableBatchDto>,
    /// A 组（可直接 attach）；CandidatesAvailable / PartialAdded 时携带，
    /// Added / AlreadyPresent 时为空 Vec。
    /// 字段与 AvailableBatchDto 同形状，独立成 DTO 便于未来扩展差异。
    pub attachable_batches: Vec<AttachableBatchDto>,
}

/// B 组候选批次（`unresolved_targets[i].available_batches[]`）。
/// part 级信息（serial_no/drawing_no/name）在 `UnresolvedTargetDto` 外层，不重复。
/// `version`：批次乐观锁版本；前端把本结构直接转发给
/// `POST /parts/batch-to-inspection` / `batch-to-ship` 的 `items[]` 时必须带上。
#[derive(Debug, Clone, Serialize)]
pub struct AvailableBatchDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    pub quantity: i32,
    pub status: BatchStatusDto,
}

/// A 组候选批次（`unresolved_targets[i].attachable_batches[]`），
/// CandidatesAvailable / PartialAdded 时随 available_batches 一起返回
/// 供前端弹窗勾选 attach（前端选中后转发到 `POST /delivery-notes/{id}/add-parts`）。
/// 字段与 AvailableBatchDto 同形状，独立成 DTO 便于未来扩展差异。
#[derive(Debug, Clone, Serialize)]
pub struct AttachableBatchDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub version: i32,
    pub quantity: i32,
    /// 仅 INSPECTION / READY_TO_SHIP（A 组定义）
    pub status: BatchStatusDto,
}

/// `POST /delivery-notes/scan` 出参（200 OK）。
///
/// 场景 → outcome 映射：
/// - `ADDED`：A 组覆盖所有 target，本次挂载 ≥1 个；`added_batches` 非空，`unresolved_targets = None`
/// - `ALREADY_PRESENT`：A 组覆盖所有 target，但都已在本单（幂等）；二者均空 / None
/// - `CANDIDATES_AVAILABLE`：散件仅 B 组；`unresolved_targets` 单元素
/// - `PARTIAL_ADDED`：装配件 A+B 混合；`added_batches` 是 A 组已挂部分，`unresolved_targets` 是 B 组子件
#[derive(Debug, Clone, Serialize)]
pub struct ScanDeliveryOut {
    pub outcome: ScanOutcomeDto,
    pub resolved: ResolvedEntityDto,
    pub note: ScanDeliveryNoteSummaryDto,

    /// 场景 ①、③、④-已挂载部分；其余场景为 `[]`
    pub added_batches: Vec<AddedBatchDto>,

    /// 场景 ②（单元素）、④（多元素）；其余场景为 `None`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved_targets: Option<Vec<UnresolvedTargetDto>>,
}