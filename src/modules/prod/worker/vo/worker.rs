//! worker 域 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

/// 工人详情出参。`work_type_name` 由 service 层用 `WorkTypeRepo::list_by_ids`
/// 单条 SQL 批量补全（防 N+1）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub badge_code: String,
    pub name: String,
    pub id_card_no: Option<String>,
    pub phone: Option<String>,
    pub is_active: bool,
    pub work_type_id: Option<String>,
    pub work_type_name: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 工人列表出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerListOut {
    pub items: Vec<WorkerOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}
