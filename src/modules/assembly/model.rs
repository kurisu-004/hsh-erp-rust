//! assembly 域数据模型
//!
//! 对应 Python myERP/model/assembly.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只需 FromRow 行结构以承载 sqlx 反序列化；
//! 金额列（unit_price / total_price NUMERIC）和日期列留到 assembly 域业务实施阶段
//! 补全，避免在当前 `Cargo.toml` 还未挂 `rust_decimal` feature 的情况下
//! 引入编译期拒收。

use chrono::NaiveDateTime;

/// `t_assembly` 行（Phase P1 投影）
///
/// 仅含 delivery_note / delivery_group 当前会用到的列：
/// 标识 + 序列号 + 图号 + 名称 + 客户 + 状态 + 订单号 + 乐观锁 + 软删。
/// 金额列与日期列在 assembly 域实施阶段扩展。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TAssembly {
    pub id: i64,
    pub drawing_no: String,
    pub name: String,
    pub customer_id: i64,
    pub status: String,
    pub serial_no: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub order_no: Option<String>,
}