//! iam 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。
//! 按端点语义拆为 `session.rs`（CurrentUserOut / LoginResponse / LogoutResponse） +
//! `account.rs`（UserOut / UserRoleOut / UserListOut） + `menu.rs`（MenuNodeOut）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止** 出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。

// 2026-09-22 PR1：标记为 vo/ 模板源。PR4 全域复制本目录结构时以此为基准。

pub mod account;
pub mod menu;
pub mod session;

pub use account::{UserListOut, UserOut, UserRoleOut};
pub use menu::MenuNodeOut;
pub use session::{CurrentUserOut, LoginResponse, LogoutResponse};