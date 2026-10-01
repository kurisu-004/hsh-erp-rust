//! prod::shelf_process 子模块响应 VO（HTTP 返回值隔离层）
//!
//! 2026-10-02 域归属反转：自 `src/modules/shelf/vo/process_mapping.rs` 整文件平移
//! （零 diff）——货架↔工序映射的出参随端点搬到 `prod` 子模块。
//!
//! ## 与同目录 `dto.rs` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

use serde::Serialize;

use crate::shared::types::serialize_i64;

/// 单个 shelf ↔ process 映射行（按 sort_order）。
///
/// 2026-10-02：自 `src/modules/shelf/vo/process_mapping.rs` 整文件平移。
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
/// 2026-10-02：自 `src/modules/shelf/vo/process_mapping.rs` 整文件平移。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfProcessMappingOut {
    pub items: Vec<ShelfProcessMappingItem>,
}

/// 所有 shelf ↔ process 映射（GET /prod/shelf-processes 的批量查询返回）。
///
/// 用途：part_batch / worker_pool 在创建批次/工人时一次性拿全 active shelf 的
/// 工序映射，避免 N+1。
///
/// 2026-10-02：自 `src/modules/shelf/vo/process_mapping.rs` 整文件平移。
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
/// 2026-10-02：自 `src/modules/shelf/vo/process_mapping.rs` 整文件平移。
#[derive(Debug, Clone, Serialize)]
pub struct AllShelfProcessMappingOut {
    pub items: Vec<AllShelfProcessMappingItem>,
}
