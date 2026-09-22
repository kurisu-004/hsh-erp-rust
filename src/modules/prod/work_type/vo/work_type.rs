//! work_type 域 工种端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

/// 工种详情出参。`process_ids` 由 service 层用
/// `WorkTypeProcessRepo::list_by_work_types_batch` 单条 SQL 批量补全（防 N+1）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub sort_order: i32,
    pub max_held_batches: Option<i32>,
    /// 该工种被映射的工序 id 列表（JSON 序列化为 `["123", "456"]`）。空 = 未映射任何工序。
    pub process_ids: Vec<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 工种列表出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeListOut {
    pub items: Vec<WorkTypeOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}
