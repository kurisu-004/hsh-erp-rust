//! shelf ↔ process 映射端点响应 VO

use serde::Serialize;

use crate::shared::types::serialize_i64;

/// 单个 shelf ↔ process 映射行（按 sort_order）。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfProcessMappingItem {
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub shelf_code: String,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub sort_order: i32,
}

/// 单 shelf 的 mapping 列表响应。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfProcessMappingOut {
    pub items: Vec<ShelfProcessMappingItem>,
}

/// 所有 shelf ↔ process 映射（GET /shelves/processes 的批量查询返回）。
///
/// 用途：part_batch / worker_pool 在创建批次/工人时一次性拿全 active shelf 的
/// 工序映射，避免 N+1。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct AllShelfProcessMappingItem {
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub shelf_code: String,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
}

/// 全集 mapping 响应。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct AllShelfProcessMappingOut {
    pub items: Vec<AllShelfProcessMappingItem>,
}