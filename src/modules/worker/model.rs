//! worker 域数据模型
//!
//! 对应 Python myERP/model/worker.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只投影 delivery_note 后续会用到的列：
//! - `id` —— 司机 / 操作员定位（送货单 driver_worker_id 校验，Phase P2）
//! - `badge_code` —— 扫码台定位
//! - `name` / `is_active` —— 列表 / 校验
//! - `work_type_id` —— 校验工种（送货单 driver 必须 work_type.code='送货司机'）
//! - 完整列在 worker 域业务实施阶段扩展。

use chrono::NaiveDateTime;

/// `t_worker` 行（Phase P1 投影）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TWorker {
    pub id: i64,
    pub badge_code: String,
    pub name: String,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub work_type_id: Option<i64>,
}