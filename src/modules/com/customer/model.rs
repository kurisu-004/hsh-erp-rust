//! customer 域数据模型
//!
//! 对应 Python myERP/model/customer.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! ⚠️ **层级口径按「根 / 非根 / 叶子」三词走，禁说 L1 / L2**（2026-10-17）：客户树已放开为
//! 任意深度，「L2」在中间层节点上根本不成立。本文件只定义行结构，树形的判据（有无活
//! 子节点、是否根、路径怎么拼）在 `shared::customer`。

use chrono::NaiveDateTime;

/// `t_customer` 行
///
/// 客户是**任意深度**的树（2026-10-17 由固定两层放开，对齐部门架构）：
/// - `parent_id IS NULL` = **根**（集团）；
/// - `parent_id` 非 NULL = **非根**（分厂 / 车间 / 班组…，**任意深度**，既可能是叶子、
///   也可能是还有下级的中间层）。
///
/// `serial_prefix` 只在**根**上非空（与 `t_part.serial_no` / `t_assembly.serial_no` 派生
/// 关系），非根由所属根派生，本列写 NULL。
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
