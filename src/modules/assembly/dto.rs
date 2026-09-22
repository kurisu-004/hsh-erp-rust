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
use serde::Deserializer;
use serde::{Deserialize};

// ---------- 入参 ----------

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

#[derive(Debug, Clone, Deserialize)]
pub struct AssemblyChildRequest {
    pub name: String,
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub planned_delivery_date: Option<NaiveDate>,
    #[serde(default = "default_child_qty")]
    pub quantity: Option<i32>,
}

fn default_child_qty() -> Option<i32> {
    Some(1)
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