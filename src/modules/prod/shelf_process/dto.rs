//! prod::shelf_process 子模块 DTO（HTTP 请求入参）
//!
//! 对应 Python myERP/schema/shelf.py::SetShelfProcessesRequest。
//!
//! 2026-10-02 域归属反转：自 `src/modules/shelf/dto.rs` 平移（零 diff）——货架
//! 自身不含工序概念，工序映射契约随端点搬到 `prod` 子模块。
//!
//! ## DTO/VO 边界
//! 出参结构（`ShelfProcessMappingItem` / `ShelfProcessMappingOut` /
//! `AllShelfProcessMappingItem` / `AllShelfProcessMappingOut`）见同目录 `vo.rs`。

use serde::Deserialize;

/// set shelf processes 入参：整组替换（先软删全部旧 mapping → INSERT 新列表）。
///
/// `items` 可为空数组（= 清空映射）。每个 `{process_id, sort_order}` 的
/// `process_id` 必须现存，否则 service 层抛 20505。
#[derive(Debug, Clone, Deserialize)]
pub struct SetShelfProcessesRequest {
    pub items: Vec<SetShelfProcessesItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetShelfProcessesItem {
    #[serde(default)]
    pub process_id: String,
    #[serde(default)]
    pub sort_order: i32,
}
