//! iam 域集成测试（PR13 Phase D 拆分）
//!
//! 2 个原 test binary（iam_api / auth_middleware）合并为 1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! ## 拆分映射
//! - api.rs                ← 原 iam_api.rs
//! - middleware.rs         ← 原 auth_middleware.rs
//! - bootstrap_admin_seed.rs ← 2026-09-26 新增：seeds/admin.sql + BOOTSTRAP_ADMIN_ENABLED 门控
//! - wx_bind.rs            ← 2026-09-29 新增：`/iam/users/{id}/wx-bind` 3 端点
//!   （企业微信身份预绑定；wx-login 主链路在 `tests/wecom_login.rs`）
//! - menu_seed.rs          ← 2026-10-05 新增：seeds/menu.sql 授权矩阵回归护栏
//!   （6 用例钉住「工序工种/制定工序=仅 MANAGER，生产队列=MANAGER+CLERK，品检都看不到」：
//!   角色矩阵快照 / 4.7 段回收作用于存量行 / 幂等 / `/iam/me` 渲染树端到端 /
//!   白名单↔4.7 回收对账 + 4.0 复活名单镜像 / 4.0 复活 + 派生 id（2026-10-13
//!   `t_role_menu_pkey` 事故的回归护栏））
//! - shelf.rs              ← 2026-10-10 自 `tests/shelf/api.rs` 迁入：货架子模块的
//!   5 条 CRUD 端点（URL 自 `/api/v2/shelves/*` 硬切到 `/api/v2/iam/shelves/*`）
//! - shelf_deactivate.rs   ← 2026-10-10 自 `tests/shelf/deactivate.rs` 迁入：
//!   停用守卫回归（20503 / 20203）
//! - rbac.rs               ← 2026-10-14 新增：iam::rbac 子模块 8 端点 + 用户域
//!   角色授予/撤销联动（13 用例：角色列表 / MANAGER-only / grants 整包 / ADMIN
//!   元角色读全量写拒绝 / 菜单通配 / 自定义角色 CRUD 与改 code 级联 / ADMIN 自锁
//!   护栏 / 变更踢下线）

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod api;
mod bootstrap_admin_seed;
mod menu_seed;
mod middleware;
mod rbac;
mod shelf;
mod shelf_deactivate;
mod wx_bind;
