//! outsource 域 `GET /outsource-sendable` 端点响应 VO
//!
//! 2026-10-03 新增：可发送外协的（活跃批次 × OUTSOURCE 工序）一览。一行 =
//! 一个组合，与 quote / shipment / company 任何单一域都不是子资源，故走独立
//! 顶层前缀 `/api/v2/outsource-sendable`。
//!
//! ## 与写侧（prod 域）的契约
//! 前端拿本 VO 驱动两个写端点：
//! - `send_mode == "APPROVAL"` → `POST /prod/batches/{batch_id}/send-to-outsource`
//!   传 `quote_id`（**故本 VO 显式声明 `quote_id`**；前端 2026-10-03 前的
//!   `OutsourceSendableItem` 类型里还没有该字段）。
//! - `send_mode == "DIRECT"` → 同端点传 `direct: true` + 自 `company_options`
//!   选出的 `outsource_company_id`。
//!
//! 部分发送 / 部分接收传 `quantity`；`quantity == batch_quantity` 时传 `null`
//! （走全量语义）。

use serde::{Deserialize, Serialize};

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `GET /outsource-sendable` 分页信封。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceSendableListOut {
    pub items: Vec<OutsourceSendableItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// DIRECT 模式下的候选外协公司（`t_outsource_company_process` ∩ active 公司）。
///
/// 同一个类型既作 VO（Serialize，id 转字符串）又作 repo 行解码目标
/// （Deserialize，从 SQL `array_agg(json_build_object(...))` 解出）—— 故双派生。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutsourceCompanyOption {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
}

/// 可发送外协的一行。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceSendableItem {
    /// **`t_part_batch.version`**（批次级 OCC）。前端发送时原样回传。
    pub version: i32,
    /// `"APPROVAL"`（有已批准报价）/ `"DIRECT"`（无报价直发）。
    pub send_mode: String,
    /// 批次来源状态：`"PENDING"` / `"IN_PROCESS"`。
    pub source_status: String,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub part_serial_no: Option<String>,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    /// 可发送数量（行 = 批次，恒等于 `batch_quantity`）。
    pub quantity: i32,
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    /// `t_part_batch.quantity`。
    pub batch_quantity: i32,
    /// `t_part.planned_delivery_date`（`YYYY-MM-DD`）。
    pub planned_delivery_date: Option<String>,
    pub is_urgent: bool,
    /// 客户路径：有 L1 拼 `L1 / L2`，否则仅 L2 名，缺客户为 `null`。
    pub customer_path: Option<String>,
    /// 该货架上的 OUTSOURCE 工序（前端发送时当 `process_id` 回传）。
    #[serde(serialize_with = "serialize_i64")]
    pub next_process_id: i64,
    pub next_process_name: Option<String>,
    /// 批次所在货架 code。
    pub shelf_code: Option<String>,
    /// APPROVAL 有值 / DIRECT `null`。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub outsource_company_id: Option<i64>,
    pub outsource_company_name: Option<String>,
    /// 命中的 APPROVED 报价 id。APPROVAL 有值 / DIRECT `null`。
    /// **前端靠它决定发送时传哪个报价**（见文件头「与写侧的契约」）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub quote_id: Option<i64>,
    /// DIRECT 列出该公司工序映射的全部**活跃**公司；APPROVAL 恒为空数组。
    /// DIRECT 且该工序未映射任何活跃公司时为空数组 —— **该行仍返回**
    /// （前端 `canSend()` 据 `company_options.length >= 1` 置灰），不要在 SQL 里滤掉。
    pub company_options: Vec<OutsourceCompanyOption>,
    /// APPROVAL 报价的 Decimal 字符串；DIRECT `null`。
    pub price: Option<String>,
    /// 恒为 `"sendable"`（前端按它筛可发送集合）。
    pub status_label: String,
}
