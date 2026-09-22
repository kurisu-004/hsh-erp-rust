//! applicant 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为 `applicant.rs`（ApplicantOut / ApplicantListOut）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

// 2026-09-22 PR4：applicant 域 dto.rs 出参结构抽离到 vo/，对齐 iam 范本。

pub mod applicant;

pub use applicant::{ApplicantListOut, ApplicantOut};
