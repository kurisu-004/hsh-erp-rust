//! com 域集成测试
//!
//! cargo 1.98 auto-discover 约定：`tests/<dir>/main.rs` 作为 binary 入口，
//! sub-file 通过 `mod xxx;` 引入；缺 main.rs 时 sub-file 不会被编译为
//! 集成测试。
//!
//! ## 拆分映射
//! - union_list.rs                 ← 2026-09-29（`GET /com/union-list`）
//! ## 为什么本域目录叫 `mod.rs` 而不是 `main.rs`
//! cargo 1.98 的 test auto-discover 只认 `tests/*.rs` 与 `tests/*/main.rs`
//! （**一级**目录）。`tests/com/delivery_note/` 在两级之下，`main.rs` 不会被
//! 自动发现 ⇒ 该目录整体作为本 binary 的一个子模块引入
//! （`mod delivery_note;` → 解析到 `delivery_note/mod.rs`），与 `tests/part/mod.rs`
//! 同一形态。
//!
//! ## 本 binary 的成员
//! - union_list.rs            ← 2026-09-29（`GET /com/union-list`）
//! - delivery_note/{mod,group,scan,note}.rs ← 原 tests/delivery/*（2026-10-08 随域
//!   平移 com；URL 硬切 `/api/v2/com/delivery/*`）+ 新增 5 个文件（扫码三层树 /
//!   入单闸门 / DP 分配 / 司机候选 / 指定司机）；`attach_batches.rs` 随
//!   `POST /{id}/attach-batches` 端点删除
//! - delivery_note_in_use.rs  ← 原 tests/guard_dn_in_use_api.rs（2026-10-08 随域平移
//!   改名；内容是 part / assembly 的 21420 守卫，打的是 `POST /parts/{id}/cancel`
//!   与 `POST /assemblies/{id}/soft-delete`，**不属于本域端点**，只是文件名随域
//!   归位）

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod delivery_note;
mod delivery_note_in_use;
mod union_list;
