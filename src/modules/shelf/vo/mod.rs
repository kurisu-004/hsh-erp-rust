//! shelf 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为：
//! - `shelf.rs`（ShelfOut / ShelfListOut / ShelfForReturnItem / ShelfForReturnOut /
//!   ShelfForInspectionItem / ShelfForInspectionOut）—— 主货架 CRUD + picker
//!
//! ## 2026-10-02 域拆分
//! 原 `process_mapping.rs`（4 个货架↔工序映射 VO）已随端点搬到
//! `src/modules/prod/shelf_process/vo.rs`（同一目录下另起 `mod.rs` 分层会让
//! 单文件 VO 多一层无信息量的壳，与 `prod::batch` / `prod::process` 的平级单文件
//! 形态不一致）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

// 2026-09-22 PR4：按 iam vo/ 范本把 dto.rs 中的出参类型抽出到此目录；DTO 仅保留入参。

pub mod shelf;

pub use shelf::{
    ShelfForInspectionItem, ShelfForInspectionOut, ShelfForReturnItem, ShelfForReturnOut,
    ShelfListOut, ShelfOut,
};
