//! process_chain 域（工艺链 header + step）
//!
//! 对应 Python myERP（无对位 Python 文件；本域为新增）：
//! - service/process_chain_service.py（待后续 PR）
//! - schema/process_chain.py（待后续 PR）
//!
//! ## 当前阶段（part-worker-pool-federated-rocket MVP + 2026-09-16 FK 翻转）
//! 3 端点挂在 `/api/v2/process-chains`：
//! - `GET  /by-part/{part_id}`  —— 读 part 绑定的工艺链（404 + 20701）
//! - `PUT  /by-part/{part_id}`  —— 整组 upsert：OCC + 软删旧 steps + INSERT 新 steps
//! - `GET  /{chain_id}`         —— 按链 id 读工艺链（FK 翻转新增；404 + 20701）
//!
//! ## 子模块结构
//! - `model`        —— 表行（header + step）+ 插入行 builder
//! - `statemachine` —— 纯函数（sort_order 间隙检测 + reorder helper）
//! - `repo`         —— SELECT 在 query.rs；INSERT/UPDATE/DELETE 在 mutate.rs
//! - `service`      —— `crud.rs` 单文件（业务编排：upsert 整组事务）
//!
//! 约定沿用 backend-rust/CLAUDE.md：handler ≤ 250 行、事务边界在 handler、
//! repo 用 `impl PgExecutor<'_>`。

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;

use crate::state::AppState;
use axum::{Router, routing::get};
use std::sync::Arc;

/// process_chain 域路由表（挂载点 `/api/v2/process-chains`，见 `modules::v2_router`）。
/// 静态段 `by-part` 优先于参数段 `{chain_id}`（axum 路由匹配规则），顺序无关。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/by-part/{part_id}",
            get(handler::get_by_part).put(handler::upsert),
        )
        .route("/{chain_id}", get(handler::get_by_id))
}
