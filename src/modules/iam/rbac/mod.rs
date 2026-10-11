//! iam 域下的角色与权限模型（RBAC，8 条端点，2026-10-14 新建）
//!
//! ## 域范围
//! 「**谁能做什么**」的后半句：角色定义（`t_role`）、权限点（`t_permission`）、
//! 角色→权限点授权（`t_role_permission`），以及角色→菜单授权（`t_role_menu`，
//! 既有表，本模块读写它的授权整包）。三张表全部归 iam 域，与 `iam::session`
//! / `iam::account` / `iam::shelf` 同级 —— 它们回答的是同一类问题的不同侧面
//! （账号是谁、角色能干什么）。
//!
//! ## 8 条端点（全部 MANAGER-only）
//! | Method | Path | 说明 |
//! |---|---|---|
//! | GET | `/api/v2/iam/roles` | 角色列表（含在册用户数） |
//! | POST | `/api/v2/iam/roles` | 新建自定义角色 |
//! | POST | `/api/v2/iam/roles/{id}/update` | 改角色（改 code 时级联三表） |
//! | POST | `/api/v2/iam/roles/{id}/deactivate` | 停用 / 软删 |
//! | GET | `/api/v2/iam/roles/{id}/grants` | 读授权整包 |
//! | POST | `/api/v2/iam/roles/{id}/grants` | 授权整包替换 |
//! | GET | `/api/v2/iam/permissions` | 权限点清单（按域分组） |
//! | GET | `/api/v2/iam/menus` | 菜单树（与 `/iam/me` 的 `menus` 同形） |
//!
//! ## 为什么守卫是 `require_role(Role::Manager)` 而不是权限点
//! **防自锁**：这个面板自己就是权限真源。若用「进入面板」这个权限点守它，
//! 管理员一旦把这个点从自己身上去掉，就**再也没有人能把它加回去** ——
//! 唯一的出口（这个面板）正是被自己关掉的那个。
//!
//! ⇒ 守卫下沉一层，用**不会被本面板改动**的角色（`MANAGER`）守它：MANAGER
//!   的停用 / 删除在本模块固定 400（与 ADMIN 同等保护），面板无法把守卫角色
//!   本身关掉。它的菜单 / 权限点 grants 仍可改 —— 那只影响菜单可见性与业务
//!   端点权限，不改 `require_role` 的判定。ADMIN 用户由 `CurrentUser::has_role`
//!   的元角色通配天然放行（ADMIN 通配在 `auth::rbac`，唯一实现点）。
//!
//! 权限点真正生效的地方是**业务端点**（如 `part.drawing.print` 守图纸打印），
//! 见 `part::handler::print::require_print_role`。
//!
//! ## ADMIN 是元角色，不是数据授权
//! - **代码级通配**：`has_role` 命中 ADMIN 时对任意角色 / 任意权限点放行；
//! - **不进 `t_role_menu` / `t_role_permission`**：`GET /roles/{ADMIN}/grants`
//!   返回**全量** active 菜单码 / 权限码（面板要能整页渲染），但
//!   `POST /roles/{ADMIN}/grants` 一律 400；
//! - **ADMIN 角色不贡献权限码**（`t_role_permission` 无行）：通配在
//!   `CurrentUser::require_permission` 内做，不靠数据；纯 ADMIN 账号的出参为空
//!   vec，账号兼持其它已授权角色时仍带那些码。写满行反而会让通配悄悄降级成
//!   逐项勾选。
//!
//! ## 系统角色保护（固定 400，不做计数）
//! 1. **ADMIN 角色本身不可停用 / 删除**（本模块 `deactivate_role` 固定 400）：
//!    停用生效后（登录侧只认 `t_role.is_active = true` 的角色），停用 ADMIN =
//!    所有管理员立即失去唯一能恢复它的元角色；
//! 2. **MANAGER 角色本身不可停用 / 删除**：它是本面板的守卫角色，停用后
//!    面板对所有 MANAGER 关闭，而当前没有启用端点能把角色重新启回去。
//!
//! 其余系统角色与自定义角色可停用（`delete=false`），只有自定义角色可删
//! （`delete=true`）；**停用为终态** —— 没有启用（反向）端点，恢复只能改库。
//!
//! ## ADMIN 自锁护栏（计数）
//! 1. 撤销用户的 ADMIN 角色（`iam::service::account::role::remove_role`）；
//! 2. 停用持有 ADMIN 的用户（`…::user::deactivate_user`）；
//! 3. 更新持有 ADMIN 的用户并置 `is_active=false`（`…::user::update_user`）。
//!
//! 三者是「active 且持有 ADMIN 的用户数」**计数护栏**：会归零 → 400，且
//! **不调** `delete_all_user_sessions`。阈值是 `<= 1`（不是 `< 2`）的等价写法：
//! 两个及以上管理员时降级到 1 不构成自锁，还剩人能恢复。计数与写入之间由
//! 用户域对 `t_role` ADMIN 行的行锁串行化（`IamRepoTrait::lock_admin_role_for_update`），
//! 堵住「并发撤销两个管理员双双放行」的窗口。
//!
//! ## 停用即降权
//! 登录 / refresh / me 的角色解析只取 `t_role.is_active = true AND deleted_at IS
//! NULL` 的角色行（`resolve_roles_and_scope`），该角色的权限 / 菜单 / 货架 scope
//! 一并失效；停用 / 删除角色都会踢全部在册持有者。**停用为终态**：本模块没有
//! 启用（反向）端点，停用后如需恢复只能改库把 `is_active` 置回 `true`。
//!
//! ## 路由挂载
//! 本模块路由挂在 **`/api/v2/iam` 根**（`/iam/roles`、`/iam/permissions`、
//! `/iam/menus`），由 `iam::handler::router()` merge 进 session router，
//! **不是** `/iam/rbac/*` —— 它是 iam 域的一等公民，与 `/iam/users`（子域
//! nest）、`/iam/shelves`（子域 nest）并列。
//!
//! 本模块 8 条路由里没有裸 `/{id}` catch-all（三条角色写端点都是
//! `/roles/{id}/…` 的两段以上路径），与 `users` nest 下的 `/{id}` 由前缀隔离、
//! 互不竞争；merge 顺序不影响匹配结果（见 `iam::handler::router`）。
//!
//! ## 域隔离
//! 本模块只读 `t_role` / `t_permission` / `t_role_permission` / `t_user` /
//! `t_user_role` / `t_menu` —— 全部是 iam 域自有表，故自动落在
//! `iam_domain_depends_on_no_other_domain` 护栏内（它扫 `src/modules/iam/**`）。
//!
//! ## 踢下线的时机
//! 授权数据装在 Redis session 缓存（`CachedUserProfile.permissions` /
//! `roles`），所以**改 grants / 停用或删除角色 / 改角色 code**提交后必须踢
//! 持有者，否则本次变更对所有人无效直到 session 自然过期。仅改角色名称 / 描述
//! 不踢（不进 session 缓存）。踢人一律在 `tx.commit()` **之后**执行
//! （Redis 不在 DB 事务内），见 [`handler::kick_sessions`]。
//!
//! ## 子模块结构（照 `iam::shelf` 的嵌套子模块范本）
//! - `handler.rs` —— 8 端点 + 路由工厂
//! - `service.rs` —— `RbacService`（字段仅 `snowflake`）+ ADMIN 护栏
//! - `repo.rs` —— `RbacRepo`（ZST + 固有静态方法）SQL 真源
//! - `model.rs` —— `TRole` / `TPermission` 行投影
//! - `dto.rs` / `vo.rs` —— 入参 / 出参

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// 转发到 [`handler::router`]（仓内多数域的转发式形态）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
