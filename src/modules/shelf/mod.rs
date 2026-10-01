//! shelf 域
//!
//! 对应 Python myERP：
//! - api/v1/shelf.py
//! - service/shelf_service.py
//! - repository/shelf_repository.py
//! - model/shelf.py
//! - schema/shelf.py
//!
//! ## Phase P3+ shelf CRUD：7 端点挂在 `/api/v2/shelves`
//! 读 2：list / get
//! picker 2：list_for_return / list_for_inspection
//! 写 3 (MANAGER)：create / update / deactivate
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
//! - `handler.rs` —— 7 端点 + 路由工厂；三形态（读 / 写 / 写+post-commit）严格区分。
//! - `service/{mod, crud, picker}.rs` —— `ShelfService`（unit struct）三形态方法签名
//!   `<R: ShelfRepoTrait>(&self, mut repo: R, ...)`。
//! - `repo/{mod, sql}.rs` —— 胖 trait `ShelfRepoTrait`（10 方法 = t_shelf 全部）
//!   + `impl for &mut PgConnection`（reborrow `&mut **self`）+ `#[cfg_attr(test,
//!   mockall::automock)]` + `sql.rs` SQL 真源（ZST struct `ShelfRepo` + 10 静态方法）。
//!
//! 2026-10-02 订正：`ShelfRepo` 静态方法数 master 原写 8，实为 10（随
//! `t_shelf_process` 4 方法搬出后与 trait 方法数重新对齐）。
//!
//! ## 实施约定
//! - 事务由 handler `pool.begin()` + `tx.commit()` 收（与 20 个 handler 文件现状对齐）；
//!   service 不知事务。
//! - service 层**零跨域调用**：`t_shelf_process` 归 `prod::shelf_process`，
//!   `t_process` 存在性校验（`list_for_return` 的 `next_process_id` 占位校验）
//!   2026-10-02 一并删除（结果被立刻丢弃 + 一次多余查询）。

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
