//! upload_session 域响应 VO（HTTP 返回值隔离层）
//!
//! 2026-09-22 PR4：从 `dto.rs` 拆出全部出参结构（Serialize-only），与 dto（Deserialize-only）
//! 严格分离。Redis 域无 SQLx 表行，但内部 `SessionFile` / `SessionCredentials` 仍是
//! 「双向序列化」结构（Redis JSON 读写），故出参仍需独立 VO 类型避免污染内部结构。
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

pub mod upload_session;

pub use upload_session::{
    AllocateFileItemOut, AllocateFilesOut, CompleteFileOut, ConsumeFilesOut, DiscardOut,
    GetOrCreateOut, RemoveFilesOut, RenewOut, SessionCredentialsOut, SessionFileOut,
};