//! customer 域 CRUD 端点响应 VO
//!
//! `parent_id` 在 VO 中是 `Option<String>`（雪花 ID 字符串形态，None → JSON `null`），
//! service 层组装时 `c.parent_id.map(|v| v.to_string())` 转换；DTO 入参也是字符串，
//! 全链路 i64 不上 JSON。

use chrono::NaiveDateTime;
use serde::Serialize;

/// 客户详情出参。`parent_name` 由 service 层补全（连表查父客户的 name）。
#[derive(Debug, Clone, Serialize)]
pub struct CustomerOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
    pub parent_id: Option<String>,
    pub parent_name: Option<String>,
    pub serial_prefix: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 客户列表出参（不分页：phase 1 仅返回全部；后续加 limit/offset 后再补）。
///
/// 字段顺序对齐 Python `schema/customer.py::CustomerListOut`。
#[derive(Debug, Clone, Serialize)]
pub struct CustomerListOut {
    pub items: Vec<CustomerOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}
