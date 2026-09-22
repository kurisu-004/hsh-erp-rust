//! work_type 域 工序映射端点响应 VO（2026-09-22 PR4 重构）

use serde::Serialize;

/// 单个 work_type ↔ process 映射行（按 sort_order）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeProcessMappingItem {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub work_type_id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkTypeProcessMappingOut {
    pub items: Vec<WorkTypeProcessMappingItem>,
}
