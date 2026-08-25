//! process 域数据模型
//!
//! 对应 Python myERP/model/process.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//! - 域枚举（DB 用 varchar，应用层用 enum 校验）
//!
//! worker-pool 域新增 `TProcess` 投影（list_by_ids 用）：
//! - `id` / `code` / `name` —— 工种可用工序白名单展开后回填
//! - 完整列（category / sort_order / description / requires_approval）留待 process
//!   域业务实施阶段扩展。

use sqlx::FromRow;

/// `t_process` 行（worker-pool 投影）
#[derive(Debug, Clone, FromRow)]
pub struct TProcess {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}
