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
//!   （4 用例钉住「工序工种/制定工序=仅 MANAGER，生产队列=MANAGER+CLERK，品检都看不到」：
//!   （角色矩阵快照 / 4.7 段回收作用于存量行 / 幂等 / `/iam/me` 渲染树端到端）
//! - shelf.rs              ← 2026-10-10 自 `tests/shelf/api.rs` 迁入：货架子模块的
//!   5 条 CRUD 端点（URL 自 `/api/v2/shelves/*` 硬切到 `/api/v2/iam/shelves/*`）
//! - shelf_deactivate.rs   ← 2026-10-10 自 `tests/shelf/deactivate.rs` 迁入：
//!   停用守卫回归（20503 / 20203）

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod api;
mod bootstrap_admin_seed;
mod menu_seed;
mod middleware;
mod shelf;
mod shelf_deactivate;
mod wx_bind;
