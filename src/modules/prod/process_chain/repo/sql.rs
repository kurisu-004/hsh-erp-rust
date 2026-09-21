//! process_chain 域 ZST `ProcessChainRepo` 结构定义（SQL 真源）
//!
//! 2026-09-22 D-1 重构：原 `repo/mod.rs` 拆为 `repo/{mod, sql, mutate, query}.rs` 四件套，
//! 把 ZST struct 单独抽到本文件；固有静态方法仍按 conventions.md §2「读 vs 写」拆
//! 在 `mutate.rs`（INSERT/UPDATE/DELETE）与 `query.rs`（SELECT/JOIN）。mod.rs 通过
//! `pub use sql::ProcessChainRepo;` 重导出，让跨模块静态调用方继续走
//! `crate::modules::prod::process_chain::repo::ProcessChainRepo::xxx(&mut *conn, ...)`
//! 路径不变（11 处 cross-module 调用方，part 域 9 + worker_pool 域 1 + part/service/inspection_core 1）。
//!
//! 抽 trait 时同步新建胖 trait `ProcessChainRepoTrait`（见 `mod.rs`），直接
//! `impl for &mut PgConnection`（reborrow `&mut **self`）——handler/service 借
//! `&mut *tx` / `&mut *conn` 即可调用，零中间壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## 为什么 trait 命名为 `ProcessChainRepoTrait`（带 `Trait` 后缀）
//! 跨模块静态调用方 11 处直接走 ZST 静态方法（part 域 9 + worker_pool 域 1 +
//! part/service/inspection_core 1），本任务**不能**破坏
//! `prod::process_chain::repo::ProcessChainRepo` 作为 ZST 的对外身份，故 trait
//! 改名 `ProcessChainRepoTrait`（与 shelf / customer 范本同形）。

/// SQL 真源 ZST。trait 名为 `ProcessChainRepoTrait`（公共接口），
/// ZST 仍名 `ProcessChainRepo`（跨模块静态调用方依赖此名）。
pub struct ProcessChainRepo;