//! process 域响应 VO（2026-09-22 PR4 重构）
//!
//! 仅含 handler 返回的 output 类型；入参类型见 `super::dto`。

pub mod process;

pub use process::{ProcessListOut, ProcessOut};
