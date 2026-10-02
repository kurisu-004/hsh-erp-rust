//! part 域位置树出参 VO（`GET /api/v2/parts/location-tree`）
//!
//! 2026-10-02：原 `lifecycle.rs` 的 to-XXX / batch-to-XXX / worker-scan 出参随
//! 批次用例迁往 `crate::modules::prod::batch::vo`，本文件只剩位置树两类。

use serde::Serialize;

use crate::shared::types::serialize_i64_opt;

/// `GET /parts/location-tree` 出参：按 shelf/status 聚合的位置树。
#[derive(Debug, Clone, Serialize)]
pub struct LocationTreeNodeOut {
    pub id: String,
    pub label: String,
    pub kind: String, // "OFFICE" / "PRODUCTION_SHELF" / "WORKER" / "INSPECTION_SHELF" / "OUTSOURCE_COMPANY"
    #[serde(serialize_with = "serialize_i64_opt")]
    pub parent_id: Option<i64>,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocationTreeOut {
    pub items: Vec<LocationTreeNodeOut>,
}
