//! admin 域（对账 / 修数据的逃生口）
//!
//! 2026-10-01 新增。**本域不含任何新的业务算法**，只提供「用既有派生函数把
//! `t_part` / `t_assembly` 的派生缓存重算一遍」的能力。
//!
//! ## 端点
//! | Method | Path | 权限 | 说明 |
//! |---|---|---|---|
//! | POST | `/api/v2/admin/recompute-rollup` | **Manager** | 按 `t_part_batch → t_part → t_assembly` 重跑派生，返回 before→after 报告 |
//!
//! ## 背景：为什么需要它（2026-10-01）
//!
//! 三层状态是单向派生的（`t_part_batch.status` 是**唯一真源**，`t_part` /
//! `t_assembly` 是派生缓存），写入口已收口到
//! `part::service::status_gate::apply_batch_status_change`，并由 lib 单测
//! `status_gate::write_guard_tests::no_outside_file_writes_batch_status` 守住
//! 「除它之外无人能写批次状态」。但**收口只保证今后**，不修复历史、不覆盖人为
//! 干预：收口之前有 3 个写点漏调 sync，库里已经存在漂移行。因此提供本域作为
//! 一次性的对账 / 长期的手动收敛入口。
//!
//! ## 事务与派生顺序
//!
//! - 事务边界在 handler：每 200 行一个 `pool.begin()` / `commit()`（见
//!   `handler.rs::CHUNK_SIZE` 的取值理由），service 不知事务。
//! - part 段先于 assembly 段：父装配件的聚合读子件**当前**状态，反序会让刚
//!   修正的 part 不被计进本轮聚合。
//! - WS 广播在全部块 commit 之后（`ROLLUP_RECOMPUTED`），且**仅在真有变化时**
//!   发；幂等的空跑不发事件、不报错。

pub mod dto;
pub mod handler;
pub mod service;

use std::sync::Arc;

use axum::Router;
use axum::routing::post;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/recompute-rollup", post(handler::recompute_rollup))
}
