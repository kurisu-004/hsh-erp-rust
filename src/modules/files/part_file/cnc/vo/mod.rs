//! `part_file::cnc` 子模块响应 VO（2026-09-22 PR4 重构，2026-10-13 收编）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。
//!
//! 2026-10-13：`CncPairListItem` / `CncPairListOut` 随配对列表端点下线删除。

pub mod cnc_pair;

pub use cnc_pair::{CncFileRef, CncPairOut};
