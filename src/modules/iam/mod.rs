//! iam 域（认证 + 账号合并 + 货架实体管理）
//!
//! ## 为什么货架归 iam（2026-10-10）
//! 用户视角里，账号与货架是同一类东西 —— 「**谁能碰什么**」的权限资源：
//! - `t_user_role` 里 `SHELF_ACCOUNT` 角色的 `scope_id` **指向某个货架**：货架实体
//!   是这套权限体系的落点，不是独立业务对象。归属 iam 后「这个 scope 指向的货架还在
//!   不在」由本域 SQL 直接读 `t_shelf` 回答，不必跨域问；
//! - 5 条货架写端点全部 MANAGER 独占，与账号 / 角色管理是同一批授权动作。
//!
//! 货架实体是 iam 域的**嵌套子模块** `iam::shelf`（`src/modules/iam/shelf/`，5 端点，
//! URL `/api/v2/iam/shelves/*`），不是摊平进本域的扁平目录 —— 扁平目录里已经全是
//! 账号 / 会话文件，货架 CRUD 混进去会失焦。归属缘由与迁移记录见
//! [`shelf::mod`] 的模块 doc。
//!
//! **不归 iam 的两样东西**：工序映射 `prod::shelf_process`（`t_shelf_process` 的
//! `process_id` 指向 prod 域实体 `t_process`）与选架设施 `shared::shelf`（跨域设施层、
//! 无域归属）。
//!
//! 目录形态：`dto/` `handler/` `repo/` `service/` `vo/` 是账号与会话的扁平结构，
//! `service/account/` 是子目录先例；本域端点全挂在 `/api/v2/iam` 下。
pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod shelf;
pub mod vo;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
