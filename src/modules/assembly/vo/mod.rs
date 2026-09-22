//! assembly 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为 `assembly.rs`（AssemblyOut / AssemblyChildOut / AssemblyFileRef /
//! AssemblyListItem / AssemblyListOut / AssemblyDetail / AssemblyCreateResult）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

// 2026-09-22 PR4：按 iam vo/ 范本把 dto.rs 中的出参类型抽出到此目录；DTO 仅保留入参。

pub mod assembly;

pub use assembly::{
    AssemblyChildOut, AssemblyCreateResult, AssemblyDetail, AssemblyFileRef, AssemblyListItem,
    AssemblyListOut, AssemblyOut,
};