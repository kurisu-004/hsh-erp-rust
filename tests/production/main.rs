//! production 域集成测试（PR13 Phase D 拆分）
//!
//! 6 个原 test binary（work_type_api / process_api / process_chain_api / worker_api /
//! worker_pool_api / worker_pool_auto_allocate_api）合并为 1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! 与 CLAUDE.md `src/modules/prod/*` 支撑域对齐（work_type / process / process_chain /
//! queue / worker），对应测试文件同名。
//!
//! ## 拆分映射
//! - work_type.rs              ← 原 work_type_api.rs
//! - process.rs                ← 原 process_api.rs
//! - process_chain.rs          ← 原 process_chain_api.rs
//! - queue.rs                  ← 原 worker_pool.rs（2026-10-08 worker_pool → queue 更名）
//! - queue_dispatch.rs         ← 原 batch.rs（2026-10-08：4 个下发流 / 召回端点自 prod::batch
//!   剥离到 prod::queue 后随之改名；17 个用例全部打的是这 4 个端点，batch 域因此
//!   不再有独立文件）
//! - queue_board.rs            ← 2026-10-08 新增（prod::queue 两个只读聚合端点：
//!   ★ 核心回归是「10 个工人的工序板一次返回 10 个 worker 且 held 批次不漏不错」）
//! - queue_auto_allocate.rs     ← 原 worker_pool_auto_allocate_api.rs（2026-10-08 更名）
//! - worker.rs                 ← 原 worker_api.rs
//! - scan_badge.rs             ← 2026-10-10 新增（`prod::scan` 扫工牌
//!   `POST /prod/scan/verify-badge`；自 worker.rs 迁入其 verify-badge 场景）
//! - scan_listing.rs           ← 2026-10-10 迁入（`prod::scan` 两条只读聚合端点
//!   `GET /prod/scan/pickable` / `GET /prod/scan/held`；自 tests/part/
//!   pickable_by_work_type.rs 迁来，URL 与行 VO 一并换成 `ScanListItem`）
//! - scan_worker_scan.rs       ← 2026-10-10 自 queue.rs 搬出 worker-scan 场景
//!   （`POST /prod/scan/worker-scan`；refill 那一半仍留在 queue.rs）
//! - pending_programming.rs     ← 2026-10-01 新增（prod::programming 待编程一览 1 端点，10 场景）
//! - shelf_process.rs          ← 2026-10-02 新增（prod::shelf_process 3 端点；自
//!   tests/shelf/api.rs 迁入整组替换场景 + 补全集查询 / 20505 / 旧路径 404 场景）
//! - pickup.rs                 ← 2026-10-03 新增（pick-up 整批 + **部分领取自动拆批**，
//!   9 场景）
//! - process_design.rs         ← 2026-10-05 新增（prod::process_design 制定工序页零件
//!   列表 1 端点，8 场景；★ 核心回归是「装配件子件可见」，锁死不加
//!   `AND assembly_id IS NULL` 守卫）
//! - inspection.rs            ← 2026-10-05 新增（prod::inspection 扫码查询 1 端点，
//!   13 场景；★ 核心回归是「扫子件 → 返回整棵装配件树（全部子件 + 全部批次）」）
//!
//! 2026-10-08：worker_pool → queue 更名 + 端点重组（详见 src/modules/prod/queue/mod.rs
//! 与 docs/api/queue.md）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod inspection;
mod pending_programming;
mod pickup;
mod process;
mod process_chain;
mod process_design;
mod queue;
mod queue_auto_allocate;
mod queue_board;
mod queue_dispatch;
mod scan_badge;
mod scan_listing;
mod scan_worker_scan;
mod shelf_process;
mod work_type;
mod worker;
