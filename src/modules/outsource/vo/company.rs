//! outsource 域 company 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::serialize_i64;

/// 外协公司详情（不含工序映射）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub address: Option<String>,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 外协公司详情（含工序映射）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyWithProcessesOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub address: Option<String>,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub processes: Vec<OutsourceCompanyProcessLinkOut>,
}

/// 公司 ↔ 工序 映射出参。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyProcessLinkOut {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub process_name: String,
    pub category: String,
    pub sort_order: i32,
}

/// 外协公司列表出参。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyListOut {
    pub items: Vec<OutsourceCompanyOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}