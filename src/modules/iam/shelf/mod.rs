//! iam 域下的货架实体管理（5 条 CRUD 端点）
//!
//! ## 域归属：为什么货架归 iam（2026-10-10）
//! 用户视角里，账号与货架是同一类东西 —— 「**谁能碰什么**」的权限资源：
//! - 账号的权限来自 `t_user_role`，其中 `SHELF_ACCOUNT` 角色的 `scope_id`
//!   **指向某个货架**：一个账号被授予的货架权限就是它的作用域，货架实体是这套
//!   权限体系的落点而不是独立业务对象；
//! - 货架的写端点全部 MANAGER 角色专属，与账号 / 角色管理同一批授权动作。
//!
//! 归属 iam 后，「谁能操作货架」与「谁能操作账号」在同一域内同一条 SQL 链路里，
//! 不必跨域问「这个 scope_id 指向的货架还在不在」——本模块的 SQL 直接读 `t_shelf`。
//!
//! ## 2026-10-10 自 `crate::modules::shelf` 迁入 iam
//! 目录与路由同时搬迁：
//! - 源码：`src/modules/shelf/` → `src/modules/iam/shelf/`
//! - URL：`/api/v2/shelves/*` → `/api/v2/iam/shelves/*`，**硬切无 alias**，
//!   旧路径 404
//!
//! 端点的请求 / 响应契约**逐字未变**（含错误码 20501 / 20502 / 20503 / 20512 与
//! OCC `version` 语义），只有 URL 前缀变了。本模块目录形态是**嵌套子模块**而非
//! 摊平进 iam 的扁平目录：iam 的 `dto/` `handler/` `repo/` `service/` `vo/` 里
//! 已全是账号/会话文件，货架 CRUD 摊进去会与之混作一团。
//!
//! ### 为什么不搬 `prod::shelf_process`
//! 工序映射（`t_shelf_process`）关联的是 prod 域实体 `t_process`，它的
//! `process_id` 引用工序表、端点也是围绕「某工序绑到某货架」组织，属生产语义；
//! 它在 2026-10-02 的 shelf 域拆分里已经从本模块搬进
//! `src/modules/prod/shelf_process/`，URL 硬切到 `/api/v2/prod/shelf-processes/*`。
//! 那次拆分之后本模块只剩纯 `t_shelf` 自身操作。
//!
//! ### 为什么不搬 `shared::shelf`
//! `shared::shelf` 是**选架设施**（负载聚合 `load` + 自动选架 `select`），被
//! part / prod / outsource 多个域共同调用，属跨域设施层，按仓库约定不上带域归属。
//! 它与「货架实体怎么维护」是两件事：设施层只读货架表，不改它。
//!
//! ## 5 端点
//! 读 2：list / get
//! 写 3 (MANAGER)：create / update / deactivate
//!
//! `GET /for-return` 与 `GET /for-inspection` 两条 picker 端点已**下线
//! （无 alias）**：货架改由服务端按负载自动选（`shared::shelf::select`），前端不再
//! 需要「挑一个架」这个动作。移除记录与替代者见 `docs/api/shelves.md`。
//!
//! ## 路由注册顺序（2026-10-10）
//! `/{id}` 是 catch-all，必须排在同层的静态兄弟段之后（axum matchit 0.8 按注册顺序
//! 消歧）。picker 下线后本模块只剩 `/` 与 `/{id}` / `/{id}/update` / `/{id}/deactivate`
//! 五条路由，后两条是**两段**路径、与 `/{id}` 不同形，故顺序上无约束 —— 但若将来再加
//! 单段静态路径（如 `/search`），必须插在 `/{id}` **之前**。
//!
//! 注意这层 catch-all 现在嵌在 `/iam/shelves/` 前缀之下：形如
//! `/api/v2/iam/shelves/for-return` 的请求会落进 `/{id}` 的 `Path<i64>` 提取器并
//! 拿到 **400 纯文本**（不是 404），而**不带 `/iam` 前缀**的旧路径才是干净的 404。
//!
//! ## 2026-10-02 域拆分（3 个 mapping 端点 + account_count 出本模块）
//! - **工序映射**（`GET|POST /shelves/{id}/processes` + `GET /shelves/processes`）
//!   搬到 `src/modules/prod/shelf_process/`，URL 硬切
//!   `/api/v2/prod/shelf-processes/*`（**无 alias**，旧路径 404）
//! - **账号部分**消除：`ShelfOut.account_count` + `count_accounts_by_shelf` 删除，
//!   货架↔账号绑定的真源在 iam 域 `t_user_role`，本模块零改动
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
//!   2026-10-02 一并删除（结果被立刻丢弃 + 一次多余查询）。本模块也
//!   **不 import `crate::shared::shelf`** 的选架函数 —— `capacity` / `current_load` 的
//!   读走 `ShelfRepoTrait::load_by_ids`（其实现委托 shared 层），选架是**调用方**的
//!   责任，本模块只提供「被选」的货架数据。

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
