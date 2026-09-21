//! iam 域（认证 + 账号合并）
//!
//! 对应 Python myERP：
//! - api/v1/auth.py + api/v1/user.py → `handler.rs`（14 端点，1 个 router 工厂函数）
//! - service/auth_service.py + service/user_service.py + service/menu.py → `service/{session,account,menu}.rs`
//! - repository/user_repository.py + repository/menu_repository.py + repository/shelf_repository.py
//!   → `repo/sql.rs`（SQL 真源）+ `repo/pg.rs`（PgIamRepo 借连接）+ `repo/mod.rs`（IamRepo trait）
//! - model/user.py + model/menu.py → `model.rs`
//! - schema/auth.py + schema/user.py → `dto.rs`
//!
//! 路由挂载点（PR-4 收尾后唯一对外接口，见 `modules::v2_router`）：
//! - `/api/v2/iam`         IAM 域唯一对外接口（session 端点在根；account 端点在 `/iam/users`）
//!
//! 旧 alias `/api/v2/auth/*` + `/api/v2/users/*` 已于 2026-09-19 PR-4 下线，
//! `iam::handler::auth_router` / `iam::handler::users_router` 两个旧工厂函数随本任务一并移除。
//!
//! 全部端点要求 MANAGER 角色（除 session 端点 `login` / `refresh` 是公开，`me` / `logout` /
//! `change-password` 仅需登录），权限守卫在 service 层（见 `handler.rs` 头注释）。
//!
//! ## 实施约定
//! - 横切的 JWT 编解码、CurrentUser extractor、RBAC 角色枚举均在顶层 `crate::auth`，
//!   本模块仅承载业务 iam handler / service / repo / dto。
//! - service 拆分（conventions §4.2 / §2 1000 行硬约束）：
//!   `service::SessionService`（登录 / refresh / me / logout / change-password） +
//!   `service::AccountService`（账号 CRUD / 角色 / 改密）+ `service::build_menu_tree`（纯函数）。
//! - repo（2026-09-21 重构后）：`repo/mod.rs` 内胖 trait `IamRepo`（17 方法合并单 trait，
//!   因 `&mut PgConnection` 同一作用域只能借给一个 repo）+ `PgIamRepo<'a>` 借连接实现 +
//!   `repo/sql.rs` 内固有静态方法 SQL 真源。
//!
//! 2026-09-19 IAM 域合并（PR-1）：合并 `auth` + `user` 两个垂直切片为单一 `iam` 域。
//! 2026-09-19 IAM 域收尾（PR-4）：删除旧 alias `/auth` + `/users` 兼容期 nest，
//! `/api/v2/iam/*` 成为 IAM 域唯一对外接口。JWT 契约 / Redis session / DB schema 零修改。
//! 2026-09-21 事务分层重构：删 `uow.rs`，事务移交 handler，service 借 `repo: &mut R`
//! 参数化；胖 trait + PgIamRepo 借连接 + MockIamRepo 单测注入。

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;

// 单测子模块：拆分到 service_tests/ 目录（account_tests + session_tests）以避开 service.rs
// 1000 行上限。`mod.rs` 顶层只声明 `pub mod service_tests`，handler 集成测试在
// `tests/iam_api.rs`。
#[cfg(test)]
mod service_tests;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
