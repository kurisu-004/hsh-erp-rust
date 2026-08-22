//! part 域数据模型
//!
//! 对应 Python myERP/model/part.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只需 FromRow 行结构以承载 sqlx 反序列化；
//! 域枚举（PartStatus / PartLocation / PartEventType 等）的 Rust enum 等到
//! part 域业务实现阶段再补，避免越权改动本域。
//!
//! 完整列（含金额 / 数量 / holder / 日期 / note / has_been_repaired 等）待
//! part 域业务实施时再补全 —— 当前 `Cargo.toml` 还未挂 `rust_decimal` feature，
//! 而 `unit_price` / `total_price` 是 NUMERIC，缺 feature 时 sqlx 编译期拒收。

use chrono::NaiveDateTime;

/// `t_part` 行（Phase P1 投影）
///
/// 仅含 delivery_note / delivery_group 当前会用到的列：
/// 标识 + 序列号 + 客户 + 装配件 + 状态 + 送货单 + 乐观锁 + 软删。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPart {
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub customer_id: i64,
    pub assembly_id: Option<i64>,
    pub status: String,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub delivery_note_id: Option<i64>,
}