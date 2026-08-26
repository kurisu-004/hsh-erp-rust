//! applicant 域数据模型
//!
//! 对应 Python myERP/model/applicant.py。
//! 表已在 migrations/20260811100002_002_create_customer_tables.sql 建好。

use chrono::NaiveDateTime;

/// `t_applicant` 行结构。
///
/// `customer_id` 逻辑外键 → `t_customer.id`（必须是 L1，service 校验）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TApplicant {
    pub id: i64,
    pub name: String,
    pub customer_id: i64,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}
