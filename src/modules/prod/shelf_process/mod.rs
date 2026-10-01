//! prod::shelf_process 子模块 —— 货架 ↔ 工序映射（`t_shelf_process`）
//!
//! 3 端点全部挂 `/api/v2/prod/shelf-processes`（2026-10-02 自 shelf 域硬切）：
//! - `GET  /api/v2/prod/shelf-processes`              —— 全集 mapping（防 N+1）
//! - `GET  /api/v2/prod/shelf-processes/{shelf_id}`   —— 单架 mapping
//! - `POST /api/v2/prod/shelf-processes/{shelf_id}`   —— 整组替换（MANAGER）
//!
//! 旧路径 `GET|POST /api/v2/shelves/{id}/processes` 与 `GET /api/v2/shelves/processes`
//! **不再挂载**（404，无 alias；沿 2026-09-19 prod 聚合先例）。请求 / 响应契约逐字
//! 不变，前端只改 URL。
//!
//! ## 为什么搬到 prod（2026-10-02 域拆分）
//! 「货架在后端现在是单独的模块，但货架自身包括了账号的部分和工序映射相关的部分，
//! 按照后端的规约应该拆分到 iam 域和 prod 中」：
//! - **账号部分 = 消除**：`ShelfOut.account_count`（查 `t_user_role WHERE
//!   scope_type='shelf'`）唯一喂养方已删，绑定真源本来就在 iam 域，shelf 域零改动
//! - **工序映射 = 搬进 prod**：`t_shelf_process` 关联的是 `t_process`（prod 域实体），
//!   放在 shelf 域造成 shelf → prod 的反向依赖；搬进 prod 后方向翻转为
//!   prod → shelf（只读 `ShelfRepo::get_by_id` 校验 shelf 存在 / scope）
//!
//! ## 子模块结构（与 `prod::batch` / `prod::work_type` 同形）
//! - `dto.rs` —— 入参（`SetShelfProcessesRequest` / `SetShelfProcessesItem`）
//! - `vo.rs` —— 出参（`ShelfProcessMappingItem` / `ShelfProcessMappingOut` /
//!   `AllShelfProcessMappingItem` / `AllShelfProcessMappingOut`）
//! - `repo.rs` —— `t_shelf_process` SQL 真源（ZST `ShelfProcessRepo` + 6 静态方法：
//!   平移 4 + 2026-10-02 收口 `prod::batch` / `prod::worker_pool` 各 1 处）+ `NewShelfProcessRow`
//! - `service.rs` —— `ShelfProcessService`（3 方法：`set_shelf_processes` /
//!   `list_all_mappings` / `list_shelf_processes`），事务边界在 handler
//! - `handler.rs` —— 3 端点 + 路由工厂
//!
//! ## 与 shelf 域的关系
//! shelf 域（`src/modules/shelf/`）保留 7 端点（CRUD 4 + picker 2 + 列表 / 详情），
//! 其 `ShelfRepoTrait` 已缩到 10 个纯 `t_shelf` 方法，`t_shelf_process` 与 2 个
//! 反向 helper 全部删除。

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
