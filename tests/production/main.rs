//! production 域集成测试（PR13 Phase D 拆分）
//!
//! 6 个原 test binary（work_type_api / process_api / process_chain_api / worker_api /
//! worker_pool_api / worker_pool_auto_allocate_api）合并为 1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! 与 CLAUDE.md `src/modules/prod/*` 5 支撑域对齐（work_type / process / process_chain /
//! worker_pool / worker），对应测试文件同名。
//!
//! ## 拆分映射
//! - work_type.rs              ← 原 work_type_api.rs
//! - process.rs                ← 原 process_api.rs
//! - process_chain.rs          ← 原 process_chain_api.rs
//! - worker_pool.rs            ← 原 worker_pool_api.rs（1587 行）
//! - worker_pool_auto_allocate.rs ← 原 worker_pool_auto_allocate_api.rs
//! - worker.rs                 ← 原 worker_api.rs
//! - batch.rs                  ← 2026-09-29 新增（prod::batch 4 端点端到端 + 错误码 + 角色守卫）
//! - pending_programming.rs     ← 2026-10-01 新增（prod::programming 待编程一览 1 端点，10 场景）
//! - shelf_process.rs          ← 2026-10-02 新增（prod::shelf_process 3 端点；自
//!   tests/shelf/api.rs 迁入整组替换场景 + 补全集查询 / 20505 / 旧路径 404 场景）
//! - pickup.rs                 ← 2026-10-03 新增（pick-up 整批 + **部分领取自动拆批**，
//!   9 场景）

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod batch;
mod pending_programming;
mod pickup;
mod process;
mod process_chain;
mod shelf_process;
mod work_type;
mod worker;
mod worker_pool;
mod worker_pool_auto_allocate;
