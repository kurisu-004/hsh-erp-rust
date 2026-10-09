//! assembly 域 DTO（HTTP 请求入参）
//!
//! 对应 Python myERP/schema/assembly.py。
//!
//! 2026-09-22 PR4：原 `dto.rs` 中的出参类型（AssemblyOut / AssemblyListItem /
//! AssemblyListOut / AssemblyChildOut / AssemblyFileRef / AssemblyDetail /
//! AssemblyCreateResult）已迁移至 `vo/assembly.rs`（仅 Serialize）。本文件仅保留
//! 入参类型（仅 Deserialize）。
//!
//! ## 三态 nullable 字段
//! `AssemblyUpdateRequest` 中真正可置 NULL 的字段用 `Option<Option<T>>`：
//!   `None` = 不更新，`Some(None)` = 置 NULL，`Some(Some(v))` = 覆盖。
//! 普通可空字段（不必置 NULL 的）保持 `Option<T>`。

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::Deserializer;

// ---------- 入参 ----------

/// `POST /api/v2/prod/assemblies/{assembly_id}/force-complete` 的请求体。
///
/// 2026-10-11 新增。MANAGER **单角色**守卫（不下放 Clerk 等其它角色 —— 与零件级
/// `POST /api/v2/parts/{part_id}/force-complete` 同款，逃生通道明确不委派）。
///
/// 语义：完全绕状态机，把装配件 + 全部子件 + 全部**非 CANCELLED** 批次强推
/// `COMPLETED`。用于「实际早已送货、但没在系统录入」的工单收口；判据是 dashboard
/// 大屏的三个交期桶与柱状图 —— `t_assembly.status` 一旦真被写成 `COMPLETED`，
/// 该行必然从所有切片消失。
///
/// **不收 `version`**：逃生通道不走 OCC，并发串行化由 SQL 行锁承担
/// （与零件级 `ForceCompleteRequest` 逐字同形）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ForceCompleteRequest {
    /// 操作备注，写进 `t_part_event.note` 时加 `[FORCE_ASSEMBLY]` 前缀便于审计区分。
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AssemblyListQuery {
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub statuses: Option<Vec<String>>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
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

/// `POST /api/v2/assemblies` 的单个子件入参。
///
/// 2026-10-05 新增 `unit_price` / `total_price`：此前子件价格被 repo 层写死
/// `0`，建单入参里的子件金额静默丢失（父件 `AssemblyCreateRequest` 早有这两个
/// 字段，子件侧缺）。`None` 由 SQL 侧 `COALESCE(·, 0)` 落 0 —— `t_part` 两列是
/// `NUMERIC(12,2)` / `NUMERIC(14,2) NOT NULL DEFAULT 0`，列出现在 INSERT 列清单里
/// 就不走 DB DEFAULT。
///
/// ⚠️ `rust_decimal` 启的是 `serde-with-str`：这两个字段**只接受 JSON 字符串**
/// （`"12.50"`），传裸数字反序列化失败。
#[derive(Debug, Clone, Deserialize)]
pub struct AssemblyChildRequest {
    pub name: String,
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub planned_delivery_date: Option<NaiveDate>,
    #[serde(default = "default_child_qty")]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub unit_price: Option<Decimal>,
    #[serde(default)]
    pub total_price: Option<Decimal>,
}

fn default_child_qty() -> Option<i32> {
    Some(1)
}

/// `POST /api/v2/assemblies/{assembly_id}/children` 入参（2026-09-25 新增）。
///
/// 在已存在的装配体下追加单个子件：子件继承父件 7 个共享信息字段
///（applicant_name / request_date / order_no / system_delivery_date /
/// is_urgent / note / customer_id），planned_delivery_date 缺省继承父件。
/// quantity 必填（> 0）。
#[derive(Debug, Clone, Deserialize)]
pub struct AssemblyChildAddRequest {
    pub drawing_no: String,
    pub name: String,
    #[serde(default)]
    pub planned_delivery_date: Option<NaiveDate>,
    pub quantity: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssemblyCreateRequest {
    pub drawing_no: String,
    pub name: String,
    #[serde(default)]
    pub applicant_name: Option<String>,
    pub customer_id: String,
    #[serde(default)]
    pub request_date: Option<NaiveDate>,
    #[serde(default)]
    pub planned_delivery_date: Option<NaiveDate>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default = "default_qty")]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub unit_price: Option<Decimal>,
    #[serde(default)]
    pub total_price: Option<Decimal>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub children: Vec<AssemblyChildRequest>,
}

fn default_qty() -> Option<i32> {
    Some(1)
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AssemblyUpdateRequest {
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// 2026-09-14 Phase 3（deferred #2）：三态。
    /// None = 不更新；Some(None) = 置 NULL；Some(Some(v)) = 覆盖。
    #[serde(default, deserialize_with = "deserialize_optional_optional_str")]
    pub applicant_name: Option<Option<String>>,
    /// 三态：None 不动；Some(None) 置 NULL；Some(Some(v)) 覆盖
    #[serde(default, deserialize_with = "deserialize_optional_optional_str")]
    pub customer_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub request_date: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub planned_delivery_date: Option<Option<NaiveDate>>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_decimal")]
    pub unit_price: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_decimal")]
    pub total_price: Option<Option<Decimal>>,
    /// 2026-09-14 Phase 3（deferred #2）：三态。
    #[serde(default, deserialize_with = "deserialize_optional_optional_str")]
    pub order_no: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub system_delivery_date: Option<Option<NaiveDate>>,
    /// 2026-09-14 Phase 3（deferred #2）：三态。
    #[serde(default, deserialize_with = "deserialize_optional_optional_str")]
    pub note: Option<Option<String>>,
    pub version: i32,
}

fn deserialize_optional_optional_str<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::<String>::deserialize(d)?))
}
fn deserialize_optional_optional_date<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<NaiveDate>>, D::Error> {
    Ok(Some(Option::<NaiveDate>::deserialize(d)?))
}
fn deserialize_optional_optional_decimal<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<Decimal>>, D::Error> {
    Ok(Some(Option::<Decimal>::deserialize(d)?))
}
