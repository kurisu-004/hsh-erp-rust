//! shelf 域
//!
//! 对应 Python myERP：
//! - api/v1/shelf.py
//! - service/shelf_service.py
//! - repository/shelf_repository.py
//! - model/shelf.py
//! - schema/shelf.py
//!
//! ## Phase P3+ shelf CRUD：11 端点挂在 `/api/v2/shelves`
//! 读 3：list / get / list_shelf_processes
//! picker 3：list_for_return / list_for_inspection / list_all_process_mappings
//! 写 5 (MANAGER)：create / update / deactivate / set_shelf_processes
//!
//! ## 子模块结构（2026-09-22 重构对齐 iam 范本）
//! - `handler.rs` —— 11 端点 + 路由工厂；三形态（读 / 写 / 写+post-commit）严格区分。
//! - `service/{mod, crud, picker}.rs` —— `ShelfService`（unit struct）三形态方法签名
//!   `<R: ShelfRepo>(&self, mut repo: R, ...)`；跨域 `ProcessRepo` 静态调用额外收
//!   `&mut PgConnection`。
//! - `repo/{mod, sql}.rs` —— 胖 trait `ShelfRepo`（12 方法 = t_shelf 8 + t_shelf_process 4）
//!   + `impl for &mut PgConnection`（reborrow `&mut **self`）+ `#[cfg_attr(test,
//!   mockall::automock)]` + `sql.rs` SQL 真源（ZST struct `ShelfRepo` + 8 静态方法）。
//! - `process_mapping/{mod, sql}.rs` —— `t_shelf_process` 的 SQL 真源
//!   （ZST struct `ShelfProcessRepo` + 4 静态方法）+ `NewShelfProcessRow` +
//!   `ShelfProcessService`（2 方法）。
//!
//! ## 实施约定
//! - 事务由 handler `pool.begin()` + `tx.commit()` 收（与 20 个 handler 文件现状对齐）；
//!   service 不知事务。
//! - 胖 trait `ShelfRepo` 把 t_shelf_process 也合并进来（决策方案 A，单借位 / 单 mock）；
//!   trait impl 一行委托到 `process_mapping::sql::ShelfProcessRepo` 的静态方法。
//! - 跨域调用（prod 域 `ProcessRepo`）走静态路径 `&mut PgConnection`，与 iam 范本兼容。

pub mod dto;
pub mod handler;
pub mod model;
pub mod process_mapping;
pub mod repo;
pub mod service;
pub mod vo; // 2026-09-22 PR4：响应 VO（仅 Serialize）从 dto/ 抽出到此目录

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}