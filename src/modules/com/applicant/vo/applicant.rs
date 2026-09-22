//! applicant 域 CRUD 端点响应 VO

use chrono::NaiveDateTime;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ApplicantOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>, // 由 service 连 t_customer 补
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
