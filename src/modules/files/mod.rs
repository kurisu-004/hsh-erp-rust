//! files 域 —— **文件资源容器域**
//!
//! 与 `iam`（含嵌套子模块 `iam::shelf`）、`com`、`prod` 同一形态：一个容器域，
//! 底下挂若干嵌套子模块，各自带自己的 repo / service / 路由表。
//!
//! ## 端点表（6 条，全部挂在 `/api/v2/files/*`）
//!
//! | 方法 | URL | 角色 | 归属子模块 |
//! |---|---|---|---|
//! | `POST` | `/api/v2/files/sts-tmp-keys` | M / C / CNC / I | 本文件（`handler.rs`） |
//! | `GET` | `/api/v2/files/objects` | M / C / I / CNC / **SHELF** | `part_file` |
//! | `GET` | `/api/v2/files/objects/{id}/url` | M / C / I / CNC | `part_file` |
//! | `GET` | `/api/v2/files/objects/{id}/content` | M / C / I / CNC / **SHELF** | `part_file` |
//! | `POST` | `/api/v2/files/objects/{id}/delete` | 按 kind 派生 | `part_file` |
//! | `POST` | `/api/v2/files/cnc/pairs` | M / CNC | `part_file::cnc` |
//!
//! ### ⚠️ 列表端点**不带尾斜杠**（`GET /api/v2/files/objects`，不是 `.../objects/`）
//!
//! axum 0.8 的 `nest` 把内层 `.route("/", …)` 注册成 `<prefix>` 本身，故
//! `GET /api/v2/files/objects/` 是 **404**。⚠️ 这**不是**本次重组引入的行为：
//! 迁移前的 `GET /api/v2/part-files/` 同样是 404（2026-10-13 用等价路由形状做过
//! old/new 逐条对照探针，两侧行为逐条一致）。端点映射表里写的
//! 「`GET /part-files/` → `GET /files/objects/`」中的尾斜杠来自路由注册串
//! `.route("/", …)`，**不是 wire 上的 URL**。前端照旧用**无尾斜杠**即可；
//! 该形态由 `tests/files/part_file.rs::old_part_files_prefix_returns_404` 里的
//! 正向对照（200）+ 尾斜杠对照（404）双向钉死。
//!
//! 各端点的逐字段契约、角色矩阵理由、移除记录见 `part_file/mod.rs` 与
//! `part_file/cnc/mod.rs` 的模块 doc。
//!
//! ## ⚠️ 硬切记录（2026-10-13）
//!
//! 本次重组把两个独立域收编进来，URL 一律**硬切、无 alias、旧前缀 404**：
//!
//! | 旧 URL | 新 URL | 处置 |
//! |---|---|---|
//! | `GET /api/v2/part-files` | `GET /api/v2/files/objects` | 改名 |
//! | `GET /api/v2/part-files/{id}/url` | `GET /api/v2/files/objects/{id}/url` | 改名 |
//! | `GET /api/v2/part-files/{id}/content` | `GET /api/v2/files/objects/{id}/content` | 改名 |
//! | `POST /api/v2/part-files/{id}/delete` | `POST /api/v2/files/objects/{id}/delete` | 改名 |
//! | `POST /api/v2/cnc-programs/pairs` | `POST /api/v2/files/cnc/pairs` | 改名 |
//!
//! 同批**删除 12 条零调用方端点**（part_file 域 8 条 + cnc 域 4 条），逐条清单见
//! `part_file/handler.rs` 与 `part_file/cnc/mod.rs` 的模块 doc。
//!
//! 2026-09-28 之前的历史（保留供溯源）：本域最初只是 1 条薄壳鉴权转发端点 ——
//! `POST /api/v2/files/sts-tmp-keys`。背景是上轮「移除 STS session 设施」完成后，
//! 前端 `grantStsTmpKey` / `grantStsTmpKeyFiles` 直接打到 python
//! `POST /api/v1/files/sts-tmp-keys` → nginx → python `backend:8000`，
//! **未经过 backend-rust 鉴权**。修复方式：rust 端挂同 URL 端点，
//! `CurrentUser` extractor + `require_any_role([Manager/Clerk/CncProgrammer/Inspector])`
//! 强制 JWT 鉴权，通过后透明转发 body 到 python，把响应（status + headers + body）
//! 原样透传回前端；python 端**继续裸开** by design + 部署层隔离。
//! 2026-09-29 追加：转发前注入 `X-Forwarded-User-Id: <CurrentUser.id>` 头。
//! python 端 STS service 信任此 header 取得真实 user_id。

mod handler;

pub mod part_file;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// files 域路由工厂（转发式：路由表在 `handler::router()`）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
