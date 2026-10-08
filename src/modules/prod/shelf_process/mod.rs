//! prod::shelf_process 子模块 —— 货架 ↔ 工序映射（`t_shelf_process`）
//!
//! 3 端点全部挂 `/api/v2/prod/shelf-processes`（2026-10-02 自 shelf 域硬切）：
//! - `GET  /api/v2/prod/shelf-processes`              —— 全集 mapping（防 N+1）
//! - `GET  /api/v2/prod/shelf-processes/{shelf_id}`   —— 单架 mapping
//! - `POST /api/v2/prod/shelf-processes/{shelf_id}`   —— 整组替换（MANAGER）
//!
//! 旧路径 `GET|POST /api/v2/shelves/{id}/processes` 与 `GET /api/v2/shelves/processes`
//! **不再挂载**（404，无 alias；沿 2026-09-19 prod 聚合先例）。请求 / 响应契约逐字
//! 不变，前端只改 URL。⚠️ 2026-10-10 起这三条的 404 成因变了：`/api/v2/shelves`
//! 整段前缀随货架子模块迁入 iam 域一并下线，不再是「本 router 少一条路由」而是
//! 「前缀整体不存在」—— 响应形态不变（都是 404），原因不同。
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
//!   平移 4 + 2026-10-02 收口 `prod::batch` / `prod::queue` 各 1 处）+ `NewShelfProcessRow`
//! - `service.rs` —— `ShelfProcessService`（3 方法：`set_shelf_processes` /
//!   `list_all_mappings` / `list_shelf_processes`），事务边界在 handler
//! - `handler.rs` —— 3 端点 + 路由工厂
//!
//! ## 与货架实体的关系（2026-10-10）
//! 货架实体本身是 **iam 域下的嵌套子模块** `iam::shelf`（`src/modules/iam/shelf/`，
//! 5 端点、URL `/api/v2/iam/shelves/*`）。本子模块跨域只读它的 `ShelfRepo::get_by_id`
//! 校验货架存在 / 停用 / zone —— `t_shelf_process` 归 prod、`t_shelf` 归 iam，两者的
//! 切分依据就是「哪张表的外键指向本域实体」。

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
