//! assembly 域 DTO
//!
//! 对应 Python myERP/schema/assembly.py。
//!
//! ## id 序列化约定
//! i64 字段 `id` / `customer_id` / `created_children` 用 `serialize_i64` / `serialize_i64_opt`；
//! 入参 `customer_id` 用 `String`（service 层 parse）。
//!
//! ## 三态 nullable 字段
//! `AssemblyUpdateRequest` 中真正可置 NULL 的字段用 `Option<Option<T>>`：
//!   `None` = 不更新，`Some(None)` = 置 NULL，`Some(Some(v))` = 覆盖。
//! 普通可空字段（不必置 NULL 的）保持 `Option<T>`。

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

// ---------- 出参 ----------

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub drawing_no: String,
    pub name: String,
    pub applicant_name: Option<String>,
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub request_date: Option<NaiveDate>,
    pub planned_delivery_date: Option<NaiveDate>,
    pub actual_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    pub status: String,
    pub version: i32,
    pub serial_no: Option<String>,
    pub quantity: i32,
    pub unit_price: Option<Decimal>,
    pub total_price: Option<Decimal>,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<NaiveDate>,
    pub note: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyListItem {
    #[serde(flatten)]
    pub assembly: AssemblyOut,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyListOut {
    pub items: Vec<AssemblyListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyChildOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: Option<String>,
    pub status: String,
    pub version: i32,
    pub quantity: i32,
    pub planned_delivery_date: Option<NaiveDate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyFileRef {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub original_filename: String,
    pub page_count: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyDetail {
    #[serde(flatten)]
    pub assembly: AssemblyOut,
    pub children: Vec<AssemblyChildOut>,
    pub files: Vec<AssemblyFileRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyCreateResult {
    pub assembly: AssemblyOut,
    pub created_children: Vec<AssemblyChildOut>,
}

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

fn default_child_qty() -> Option<i32> { Some(1) }

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

fn default_qty() -> Option<i32> { Some(1) }

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AssemblyUpdateRequest {
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub applicant_name: Option<String>,
    /// 三态：None 不动；Some(None) 置 NULL；Some(Some(v)) 覆盖
    #[serde(default, deserialize_with = "deserialize_optional_optional_str")]
    pub customer_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub request_date: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub planned_delivery_date: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub actual_delivery_date: Option<Option<NaiveDate>>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_decimal")]
    pub unit_price: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_decimal")]
    pub total_price: Option<Option<Decimal>>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub system_delivery_date: Option<Option<NaiveDate>>,
    #[serde(default)]
    pub note: Option<String>,
    pub version: i32,
}

use serde::Deserializer;
fn deserialize_optional_optional_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::<String>::deserialize(d)?))
}
fn deserialize_optional_optional_date<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<NaiveDate>>, D::Error> {
    Ok(Some(Option::<NaiveDate>::deserialize(d)?))
}
fn deserialize_optional_optional_decimal<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<Decimal>>, D::Error> {
    Ok(Some(Option::<Decimal>::deserialize(d)?))
}
