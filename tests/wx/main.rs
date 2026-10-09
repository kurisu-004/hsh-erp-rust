//! wx 域集成测试（2026-10-11 新增）
//!
//! wx BFF 重构 B2 的测试入口。目前只挂一个子文件：
//! - `part_list.rs` —— `wx::part_list` 子模块的 2 个端点 + `wx::login` 的响应
//!   契约 + 旧 URL 硬切的 404 回归
//!
//! 企业微信登录的 15 条既有用例仍独立成 binary（`tests/wecom_login.rs`，2026-09-29
//! 建），2026-10-11 只把 URL 从 `/wx/iam/wx-login` 改成 `/wx/login/wecom`——
//! **没有搬过来**，避免两处 helper（`bootstrap_with_wecom` / `fresh_app*`）重复维护。
//!
//! ## 命名
//! cargo 1.98 auto-discover：目录下的 `main.rs` 被当作 binary 入口，`mod.rs` 不被
//! 识别。与 `tests/production/main.rs` / `tests/iam/main.rs` 同惯例。
//!
//! ## ID 段
//! 本目录**不引入新 fixture**（沿 `tests/production/process_design.rs` 的做法：域内
//! 独享的 raw SQL 构造保留为测试文件底部的本地 `async fn`）。auth 用
//! `load_iam_fixture`（段 110+），零件 / 批次用**运行时雪花 ID**现场插，与任何
//! 常量 ID 段物理不相交。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod part_list;
