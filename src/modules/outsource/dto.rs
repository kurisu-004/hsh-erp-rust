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

use chrono::NaiveDateTime;
use serde::Deserialize;

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
// 入参 — Sendable
// ===========================================================================

/// 可发送外协的（活跃批次 × OUTSOURCE 工序）列表查询参数。
///
/// 2026-10-03 新增：`GET /outsource-sendable` 读侧契约
/// （替代 part 域错形状的 `/parts/outsource-sendable`）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OutsourceSendableListQuery {
    /// part 的 drawing_no / name ILIKE 模糊匹配。
    #[serde(default)]
    pub keyword: Option<String>,
    /// 按 `t_part.customer_id` 精确过滤。
    #[serde(default)]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ===========================================================================
// 入参 — Pool（2026-10-03 新增）
// ===========================================================================

/// `GET /outsource-pool/state` 的查询参数。
///
/// **两个参数都必填**，形态照抄 `GET /api/v2/prod/pool/state`（`worker_id` +
/// `shelf_id` 双必填）：缺任一个时 `Query` extractor 反序列化失败 → **400**
/// （axum `QueryRejection::FailedToDeserializeQuery`），而不是静默给默认值。
/// 字段写 `i64` 而非 `Option<i64>` 就是这个「必填」语义的全部实现 —— 一旦
/// 改成 `Option`，缺失参数会静默变成「不限公司 / 不限工序」，看板右列直接空掉
/// 且不报错。
///
/// 反序列化形态与既有 outsource 查询参数一致（`serde_urlencoded` 对整数字段
/// 按字符串解析，故 `?outsource_company_id=123` 合法）。
#[derive(Debug, Clone, Deserialize)]
pub struct OutsourcePoolStateQuery {
    /// 外协公司雪花 ID（= `t_part_batch.current_holder_id`，`location='OUTSOURCE_COMPANY'`）。
    pub outsource_company_id: i64,
    /// 外协工序雪花 ID（= `t_part_batch.current_process_id`）。
    pub process_id: i64,
}
