//! hsh-erp-rust 项目骨架
//!
//! **本仓库当前仅搭框架，不实现任何业务功能。**
//! 业务模块文件保留为占位符（handler/service/repo/model/dto 各自留空类型 + 注释）。
//! 详见 [`docs/architecture.md`](../docs/architecture.md) 与规划文件。

pub mod auth;
pub mod infra;
pub mod modules;
pub mod shared;
pub mod state;
pub mod task;
pub mod util;