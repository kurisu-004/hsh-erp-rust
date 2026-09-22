//! work_type 域响应 VO（2026-09-22 PR4 重构）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

pub mod process_mapping;
pub mod work_type;

pub use process_mapping::{WorkTypeProcessMappingItem, WorkTypeProcessMappingOut};
pub use work_type::{WorkTypeListOut, WorkTypeOut};
