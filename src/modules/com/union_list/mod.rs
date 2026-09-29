//! com::union_list 子模块入口（2026-09-29 新增）
//!
//! 对应 plan §1：跨表合并视图端点 `GET /api/v2/com/union-list`。
//! 镜像 `com::customer` / `com::applicant` 范本：
//! - `handler.rs`：handler + router
//! - `dto.rs`：`UnionListQuery` + `RowType` enum
//! - `repo/{mod, sql}.rs`：瘦 trait + UNION ALL + pushdown SQL
//! - `service/{mod, crud}.rs`：UnionListService 单端点业务逻辑
//! - `vo/mod.rs`：re-export `PartListItem` / `PartListOut`
//!
//! URL：`/api/v2/com/union-list`，挂载点 `com/mod.rs::router`。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
