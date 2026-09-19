//! customer 域数据模型
//!
//! 对应 Python myERP/model/customer.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只用到只读字段 + parent_id 判定 L1/L2，其他 CRUD
//! 留到 customer 域自身的实施阶段。

use chrono::NaiveDateTime;

/// `t_customer` 行
///
/// `parent_id` 为 NULL 视为 L1（一级集团）；非 NULL 视为 L2（叶子 / 分厂）。
/// `serial_prefix` 在 L1 时非空（与 `t_part.serial_no` / `t_assembly.serial_no` 派生关系），
/// L2 时由所属 L1 派生，本列写 NULL。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TCustomer {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub serial_prefix: Option<String>,
}