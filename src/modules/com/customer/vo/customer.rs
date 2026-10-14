//! customer 域 CRUD 端点响应 VO
//!
//! `parent_id` 在 VO 中是 `Option<String>`（雪花 ID 字符串形态，None → JSON `null`），
//! service 层组装时 `c.parent_id.map(|v| v.to_string())` 转换；DTO 入参也是字符串，
//! 全链路 i64 不上 JSON。

use chrono::NaiveDateTime;
use serde::Serialize;

/// 客户详情出参。`parent_name` / `customer_path` 由 service 层补全。
///
/// ⚠️ 两个「名字」字段语义不同，不要互相 cast：
/// - `parent_name` = **直接父**（上跳一层）的名字，只用于前端画树的一层缩进；
/// - `customer_path` = **第 2 层到叶子**的 `" / "` 拼接（**不含**根），与其它域
///   出参上的 `customer_path` / `l1_customer_name` 同源（`shared::customer::paths_of`）。
#[derive(Debug, Clone, Serialize)]
pub struct CustomerOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
    pub parent_id: Option<String>,
    pub parent_name: Option<String>,
    /// 2026-10-17 新增（必填键、非 nullable）：第 2 层到叶子的 `" / "` 拼接路径，
    /// **不含**根；本节点即根时 = 根自己的名字。
    ///
    /// ⚠️ **部署顺序：后端必须先上** —— 前端 `customerSchema` 把它声明成
    /// `z.string()`（必填非空），缺这个键 ⇒ `useCustomersQuery` 的守门 `parse` 抛
    /// ⇒ `data === undefined` ⇒ 客户一览页 + **全站所有客户筛选器**（零件 / 装配件 /
    /// 外协 / 送货单 / 报工台）集体空白且零红色报错。取不到时落**本节点单段名**（不是
    /// 空串），保证这个必填键永远有可读值。
    pub customer_path: String,
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
