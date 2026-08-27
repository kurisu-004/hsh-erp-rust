//! assembly 域数据模型
//!
//! 对应 Python myERP/model/assembly.py。
//! 表 schema 见 migrations/20260811100005_005_create_part_tables.sql（22 列）。

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;

/// `t_assembly` 行结构（22 列 + id = 23 字段）。
///
/// `customer_id` 逻辑外键 → `t_customer.id`（必须是 L2 叶子，service 校验）。
/// `serial_no` 由 `t_serial_counter` 派发，格式 `F0000001`；子件 `serial_no` 模式
/// `{asm_serial}-{i:02d}`（如 `F0000001-01`）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TAssembly {
    pub id: i64,
    pub drawing_no: String,
    pub name: String,
    pub applicant_name: Option<String>,
    pub customer_id: i64,
    pub request_date: Option<NaiveDate>,
    pub planned_delivery_date: Option<NaiveDate>,
    pub actual_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    pub status: String,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub serial_no: Option<String>,
    pub quantity: i32,
    pub unit_price: Option<Decimal>,
    pub total_price: Option<Decimal>,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<NaiveDate>,
    pub note: Option<String>,
}