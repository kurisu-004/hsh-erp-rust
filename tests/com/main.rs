//! com 域集成测试（2026-09-29 新增：union-list 端点覆盖）
//!
//! cargo 1.98 auto-discover 约定：`tests/<dir>/main.rs` 作为 binary 入口，
//! sub-file 通过 `mod xxx;` 引入；缺 main.rs 时 sub-file 不会被编译为
//! 集成测试。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod union_list;
