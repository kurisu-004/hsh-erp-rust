//! shelf 域数据模型占位
//!
//! 对应 Python myERP/model/shelf.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//! - 域枚举（DB 用 varchar，应用层用 enum 校验）

use sqlx::FromRow;

/// `t_shelf` 行结构（scan-inspect / fail-inspection 最小投影）。
///
/// `zone`: `'PRODUCTION'` | `'INSPECTION'`（DB varchar，无 enum 约束；
/// 应用层校验）。
#[derive(Debug, Clone, FromRow)]
pub struct TShelf {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub is_active: bool,
    pub display_order: i32,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}