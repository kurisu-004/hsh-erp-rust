//! prod::programming 子模块 —— 待编程一览（part 状态闸门 + 三规则并集口径）
//!
//! 前端「待编程一览」页的**唯一**数据源：`GET /api/v2/prod/programming/pending`。
//!
//! ## 为什么谓词要按权威列重写
//! 本端点的规则 3 读 `t_part_batch.current_process_id`（批次工序归属的唯一权威
//! 列，migration 004 确立），而非经 `t_part_batch.current_holder_id → t_shelf_process`
//! 的间接链路 —— 后者在 `t_process.is_cnc` 未正确维护 / 批次未上架时取不到工序。
//!
//! ## 过滤谓词（part 状态闸门 + 三规则并集，part 级去重）
//! 0. **状态闸门**：`p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')` —— 写在
//!    最外层，**约束全部三条规则**。因为 `t_part.process_chain_id` 从不清空，
//!    若规则2 不受状态约束，历史上挂过 CNC 链的 `COMPLETED` / `CANCELLED` /
//!    `DELIVERED` 工单会永久命中本页。
//! 1. `p.status = 'PROGRAMMING'` —— 兼容旧筛选（历史 PROGRAMMING 状态仍允许消化）
//! 2. 工单工艺链上存在 `is_cnc = TRUE` 的工序 step
//! 3. 工单存在批次，其 `current_process_id` 指向 `is_cnc = TRUE` 的工序
//!
//! 详见 [`repo`](repo.rs) 模块 doc（含 `next_process_id` 禁引用的坑）。
//!
//! ## 模块结构（与 `prod::batch` 平行）
//! - `dto.rs` —— 入参（`ProgrammingListQuery`，Query string）
//! - `vo.rs` —— 出参（`ProgrammingItemOut` / `ProgrammingListOut`）
//! - `repo.rs` —— SQL 真源（`ProgrammingRepo` ZST + `list` / `count`，共用
//!   私有 `push_where`）
//! - `service.rs` —— 业务逻辑（角色守卫 + limit/offset clamp + row→vo 投影）
//! - `handler.rs` —— HTTP 路由（只做参数提取 + `pool.acquire()` + `R::ok`）
//!
//! ## 事务 / WS 广播
//! 纯读端点：handler `pool.acquire()` 不开事务，**不发** WS 广播（无业务流转）。

use std::sync::Arc;

use axum::{Router, routing::get};

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/pending", get(handler::list_pending))
}
