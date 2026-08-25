//! part 域 DTO
//!
//! 对应 Python myERP/schema/part.py。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxOut`：单条详情出参（id 字段用 #[serde(serialize_with = shared::types::serialize_i64)]）
//! - `XxxListItem` / `XxxListOut`：列表分页
//! - `XxxListQuery`：列表查询参数（继承/字段对应 PageQuery）
//!
//! ## Phase F（pass_inspection 批量送检）
//! - `PartOut`：单件详情投影（pass_inspection 单/批端点的出参；其它端点复用做最小投影）
//! - `PassInspectionRequest`：单件入参（`POST /parts/{id}/pass-inspection`）
//! - `BatchPassItem` / `BatchPassInspectionRequest`：批量入参（`POST /parts/batch-pass-inspection`）
//! - `BatchPassFailure` / `BatchPassInspectionOut`：批量出参（含 per-item 失败明细）
//!
//! ## Phase F2（scan-inspect 一键送检 + fail-inspection 打回）
//! - `ScanDecision`：单件/批量共享的 PASS/FAIL 决策枚举
//! - `ScanInspectRequest`：`POST /parts/{id}/scan-inspect` 入参
//! - `BatchScanInspectItem` / `BatchScanInspectRequest` / `BatchScanInspectFailure` / `BatchScanInspectOut`：`POST /parts/batch-scan-inspect` 全套
//! - `FailInspectionRequest`：`POST /parts/{id}/fail-inspection` 入参（推荐需求 3）

use serde::{Deserialize, Serialize};

use crate::shared::types::{deserialize_i64, serialize_i64};

/// 工单详情投影（pass_inspection 出参；其它端点复用做最小投影）。
///
/// 字段集与 `model::TPartInspected` 完全对齐：仅含 pass_inspection 流程与最小
/// `PartOut` 响应必需列。完整业务字段（`applicant_name` / `unit_price` 等）待
/// part 域业务实施时再补全。
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
    pub actual_delivery_date: Option<chrono::NaiveDate>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
}

impl From<crate::modules::part::model::TPartInspected> for PartOut {
    fn from(p: crate::modules::part::model::TPartInspected) -> Self {
        Self {
            id: p.id,
            serial_no: p.serial_no,
            name: p.name,
            drawing_no: p.drawing_no,
            status: p.status,
            version: p.version,
            quantity: p.quantity,
            order_no: p.order_no,
            actual_delivery_date: p.actual_delivery_date,
            updated_at: p.updated_at,
            updated_by: p.updated_by,
        }
    }
}

/// 单件 pass_inspection 入参（payload 可空）。
///
/// `batch_id`：当 part 下存在多个 INSPECTION 批次（由历史部分通过产生）时，
/// caller 显式指定以消除歧义；缺省时按 part_id 唯一匹配。
/// `quantity`：本次送检数量；当前 PR 仅支持整批送检（partial-pass 拆分
/// 留待后续 PR），`quantity < target.quantity` 时返回 20111。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PassInspectionRequest {
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 批量 item（`POST /parts/batch-pass-inspection`）。
///
/// `part_id` 从 JSON 字符串反序列化（与 `serialize_i64` 对称）。`batch_id` /
/// `quantity` 语义同 [`PassInspectionRequest`]。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchPassItem {
    #[serde(deserialize_with = "deserialize_i64")]
    pub part_id: i64,
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 批量入参。`items.len()` 限制由 service 校验（[`BATCH_PASS_INSPECTION_MAX_ITEMS`]）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchPassInspectionRequest {
    pub items: Vec<BatchPassItem>,
}

/// Per-item 失败明细（item 级别错误，非整批失败）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchPassFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub code: i32,
    pub message: String,
}

/// 批量端点出参：`passed` 与 `failed` 互斥，单 item 不会同时出现在两侧。
#[derive(Debug, Clone, Serialize)]
pub struct BatchPassInspectionOut {
    pub passed: Vec<PartOut>,
    pub failed: Vec<BatchPassFailure>,
}

/// scan-inspect 第二步动作（搬到 INSPECTION 后分流）。
///
/// `PASS` → `pass_inspection_core(part_id, batch_id)`（直接复用）
/// `FAIL` → `fail_inspection_core(part_id, shelf_id, next_process_id, note, batch_id)`（推荐需求 3）
///
/// serde rename_all = "UPPERCASE"：JSON `"PASS"` / `"FAIL"` 字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ScanDecision {
    PASS,
    FAIL,
}

/// 单件 scan-inspect 入参（`POST /parts/{id}/scan-inspect`）。
///
/// `target_inspection_shelf_id`：必填；service 校验 `zone='INSPECTION'` 且 `is_active=true`。
/// `decision`：必填；FAIL 时 `shelf_id` / `next_process_id` 必填（service 校验）。
/// `batch_id`：缺省按状态唯一匹配 `{PENDING, PROGRAMMING, IN_PROCESS}` 批次；多批次歧义 → 20109。
/// `quantity`：缺省 = 整批；`quantity < target.quantity` → service 拆批（详见 service 层）。
/// `note`：≤ 500 字符；FAIL 时品检备注透传事件日志。
#[derive(Debug, Clone, Deserialize)]
pub struct ScanInspectRequest {
    pub target_inspection_shelf_id: String,
    pub decision: ScanDecision,
    #[serde(default)]
    pub shelf_id: Option<String>,
    #[serde(default)]
    pub next_process_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 批量 scan-inspect item（`POST /parts/batch-scan-inspect`）。
///
/// `decision` 缺省 = `PASS`（高频场景：装配件整组送检全 PASS）；
/// per-item `decision=FAIL` 处理"整组里特定子件需打回"边缘场景。
/// FAIL 路径要求 `shelf_id` / `next_process_id` 同时填齐。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchScanInspectItem {
    #[serde(deserialize_with = "deserialize_i64")]
    pub part_id: i64,
    #[serde(default)]
    pub decision: Option<ScanDecision>,
    #[serde(default)]
    pub shelf_id: Option<String>,
    #[serde(default)]
    pub next_process_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 批量入参。`items.len()` 限制由 service 校验（`BATCH_SCAN_INSPECT_MAX_ITEMS = 200`）。
///
/// `target_inspection_shelf_id`：批量共享一个品检架（与单件入参同形校验）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchScanInspectRequest {
    pub target_inspection_shelf_id: String,
    pub items: Vec<BatchScanInspectItem>,
}

/// Per-item 失败明细（item 级别错误，非整批失败）。
///
/// `code` 透传 service 层错误码（20103/20104/20109/20111/20511/20512/40901）；
/// `message` 透传 service 层错误文案（前端可作 toast）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchScanInspectFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub code: i32,
    pub message: String,
}

/// 批量端点出参：`submitted` 与 `failed` 互斥。
///
/// `submitted`：成功并完成 PASS/FAIL 流转的件（含 `PartOut` 最小投影）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchScanInspectOut {
    pub submitted: Vec<PartOut>,
    pub failed: Vec<BatchScanInspectFailure>,
}

/// 单件 fail-inspection 入参（`POST /parts/{id}/fail-inspection`，推荐需求 3）。
///
/// `shelf_id`：必填；目标生产货架 id（`zone='PRODUCTION'` 且 `is_active=true`）。
/// `next_process_id`：必填；下一道工序 id（与 shelf 映射）。
/// `batch_id`：缺省按状态唯一 INSPECTION 批次解析；多批次歧义 → 20109。
/// `quantity`：缺省 = 整批；部分通过走 service 拆批（`split_batch_for_partial_pass`）。
/// `note`：≤ 500 字符；品检备注透传事件日志。
#[derive(Debug, Clone, Deserialize)]
pub struct FailInspectionRequest {
    pub shelf_id: String,
    pub next_process_id: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}
