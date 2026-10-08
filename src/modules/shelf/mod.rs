//! shelf 域
//!
//! 对应 Python myERP：
//! - api/v1/shelf.py
//! - service/shelf_service.py
//! - repository/shelf_repository.py
//! - model/shelf.py
//! - schema/shelf.py
//!
//! ## Phase P3+ shelf CRUD：5 端点挂在 `/api/v2/shelves`（2026-10-10）
//! 读 2：list / get
//! 写 3 (MANAGER)：create / update / deactivate
//!
//! `GET /for-return` 与 `GET /for-inspection` 两条 picker 端点于 2026-10-10 **下线
//! （404，无 alias）**：货架改由服务端按负载自动选（`shared::shelf::select`），前端不再
//! 需要「挑一个架」这个动作。移除记录与替代者见 `docs/api/shelves.md`。
//!
//! ## 路由注册顺序（2026-10-10）
//! `/{id}` 是 catch-all，必须排在同层的静态兄弟段之后（axum matchit 0.8 按注册顺序
//! 消歧）。picker 下线后本域只剩 `/` 与 `/{id}` / `/{id}/update` / `/{id}/deactivate`
//! 五条路由，后两条是**两段**路径、与 `/{id}` 不同形，故顺序上无约束 —— 但若将来再加
//! 单段静态路径（如 `/search`），必须插在 `/{id}` **之前**。
//!
//! ## 2026-10-02 域拆分（3 个 mapping 端点 + account_count 出本域）
//! - **工序映射**（`GET|POST /shelves/{id}/processes` + `GET /shelves/processes`）
//!   搬到 `src/modules/prod/shelf_process/`，URL 硬切
//!   `/api/v2/prod/shelf-processes/*`（**无 alias**，旧路径 404）
//! - **账号部分**消除：`ShelfOut.account_count` + `count_accounts_by_shelf` 删除，
//!   货架↔账号绑定的真源在 iam 域 `t_user_role`，本域零改动
//! - `ShelfRepoTrait` 随之从 17 方法缩到 **10 方法**（纯 `t_shelf`），2 个反向
//!   跨域 helper（`proc_check_process_exists` / `proc_list_existing_process_ids`）删除
//!
//! ## 子模块结构（2026-09-22 重构对齐 iam 范本）
//! - `handler.rs` —— 5 端点 + 路由工厂；三形态（读 / 写 / 写+post-commit）严格区分。
//! - `service/{mod, crud}.rs` —— `ShelfService`（unit struct）三形态方法签名
//!   `<R: ShelfRepoTrait>(&self, mut repo: R, ...)`。
//! - `repo/{mod, sql}.rs` —— 胖 trait `ShelfRepoTrait`（9 方法 = t_shelf 全部）
//!   + `impl for &mut PgConnection`（reborrow `&mut **self`）+ `#[cfg_attr(test,
//!   mockall::automock)]` + `sql.rs` SQL 真源（ZST struct `ShelfRepo` + 9 静态方法）。
//!
//! 2026-10-02 订正：`ShelfRepo` 静态方法数 master 原写 8（随 `t_shelf_process` 4 方法
//! 搬出后与 trait 方法数重新对齐）；2026-10-10 picker 下线后再减 2 → **9**。
//!
//! ## 实施约定
//! - 事务由 handler `pool.begin()` + `tx.commit()` 收（与 20 个 handler 文件现状对齐）；
//!   service 不知事务。
//! - service 层**零跨域调用**：`t_shelf_process` 归 `prod::shelf_process`，
//!   `t_process` 存在性校验（`list_for_return` 的 `next_process_id` 占位校验）
//!   2026-10-02 一并删除（结果被立刻丢弃 + 一次多余查询）。2026-10-10 起本域也
//!   **不 import `crate::shared::shelf`** 的选架函数 —— `capacity` / `current_load` 的
//!   读走 `ShelfRepoTrait::load_by_ids`（其实现委托 shared 层），选架是**调用方**的
//!   责任，本域只提供「被选」的货架数据。

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo; // 2026-09-22 PR4：响应 VO（仅 Serialize）从 dto/ 抽出到此目录

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
