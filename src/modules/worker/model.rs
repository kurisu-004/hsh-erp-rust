//! worker 域数据模型
//!
//! 对应 Python myERP/model/worker.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//! - 完整列：id / badge_code / name / id_card_no / phone / is_active / work_type_id / 审计字段
//!
//! `id_card_no` 由 DB 部分唯一索引兜底（`uk_t_worker_id_card_no`，NULL 不参与去重），
//! 若 INSERT/UPDATE 撞索引 → `UniqueViolation`（SQLSTATE 23505）由 service 层捕获后
//! 映射为 `40901 VERSION_CONFLICT`（与 Python 保持一致；无单独的 duplicate 业务码）。
//!
//! `is_active` 与 `deleted_at` 共同追踪生命周期：`deactivate` 同时置 `is_active=false`
//! 与 `deleted_at=now()`；`reactivate` 同时置 `is_active=true` 与 `deleted_at=NULL`。
//! 这与 shelf 域 pattern 一致：扫码台（verify-badge）看到 `is_active=false` 时按
//! 20202 `BIZ_WORKER_INACTIVE` 拒绝。

use chrono::NaiveDateTime;

/// `t_worker` 行（CRUD 全投影）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TWorker {
    pub id: i64,
    pub badge_code: String,
    pub name: String,
    pub id_card_no: Option<String>,
    pub phone: Option<String>,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub work_type_id: Option<i64>,
}
