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
//! - `BatchPassItem` / `BatchPassInspectionRequest`：批量入参（`POST /parts/pass-inspection-batch`）
//! - `BatchPassFailure` / `BatchPassInspectionOut`：批量出参（含 per-item 失败明细）

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

/// 批量 item（`POST /parts/pass-inspection-batch`）。
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