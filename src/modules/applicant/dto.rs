//! applicant 域 DTO
//!
//! 对应 Python myERP/schema/applicant.py。
//!
//! ## id 序列化约定
//! i64 字段 `id` / `customer_id` 用 `serialize_i64`；Option<i64> 不存在（applicant 表无）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

// ---- 出参 ----

#[derive(Debug, Clone, Serialize)]
pub struct ApplicantOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,  // 由 service 连 t_customer 补
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicantListOut {
    pub items: Vec<ApplicantOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ---- 入参 ----

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ApplicantListQuery {
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(default)]
    pub name_like: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicantCreateRequest {
    pub name: String,
    pub customer_id: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ApplicantUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub customer_id: Option<String>,
}
