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

/// 客户列表出参。
///
/// 字段顺序对齐 Python `schema/customer.py::CustomerListOut`。
/// `limit` / `offset`：2026-10-17 起**请求不传 `limit` 即不分页、返回全量**，此时
/// `limit` 回填实际行数、`offset` 恒 0；显式传了 `limit` 才分页并原样回显。
#[derive(Debug, Clone, Serialize)]
pub struct CustomerListOut {
    pub items: Vec<CustomerOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}
