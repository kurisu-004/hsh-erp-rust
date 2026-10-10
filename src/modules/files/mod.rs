//! files 域 —— **文件资源容器域**
//!
//! 与 `iam`（含嵌套子模块 `iam::shelf`）、`com`、`prod` 同一形态：一个容器域，
//! 底下挂若干嵌套子模块，各自带自己的 repo / service / 路由表。
//!
//! ## 端点表（8 条，全部挂在 `/api/v2/files/*`）
//!
//! | 方法 | URL | 角色 | 归属子模块 |
//! |---|---|---|---|
//! | `POST` | `/api/v2/files/sts-tmp-keys` | M / C / CNC / I | 本文件（`handler.rs`） |
//! | `POST` | `/api/v2/files/objects` | M / C / CNC | `part_file`（上传四合一） |
//! | `GET` | `/api/v2/files/objects` | M / C / I / CNC / **SHELF** | `part_file` |
//! | `POST` | `/api/v2/files/objects/confirm` | M / C | `part_file`（STS 直传回执） |
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
//! ### 第二批（2026-10-13 PR1b）：part / assembly 的文件端点收编
//!
//! 本域自此是**全仓唯一**持有 `t_part_file` 写入路径的域（旧 part 域 3 条 +
//! assembly 域 1 条上传端点 + part 域 1 条 confirm 端点，共 **6** 条 → **2** 条）：
//!
//! | 旧 URL | 新 URL | 处置 |
//! |---|---|---|
//! | `POST /api/v2/parts/{part_id}/upload-drawing` | `POST /api/v2/files/objects` | 四合一 |
//! | `POST /api/v2/parts/{part_id}/upload-3d-model` | `POST /api/v2/files/objects` | 四合一 |
//! | `POST /api/v2/assemblies/{assembly_id}/files` | `POST /api/v2/files/objects` | 四合一 |
//! | `POST /api/v2/part-files/`（PR1a 下线，本轮复活并通用化） | `POST /api/v2/files/objects` | 四合一 |
//! | `POST /api/v2/parts/{part_id}/files/confirm` | `POST /api/v2/files/objects/confirm` | 搬家 + body 加 `owner_kind` / `owner_id` |
//! | `GET /api/v2/assemblies/{assembly_id}/files` | — | 删（0 调用方） |
//!
//! **6 条旧端点 → 2 条**，均无 alias、旧路径 404。
//! （2026-10-13 review 第 1 轮 NIT-11 订正：原文写「五条旧路径」，但上表 6 行；
//! 且「路径」与「端点」混用 —— 搬家类 1 条、删除类 1 条、四合一类 4 条。）
//!
//! ### ⚠️ COS 对象 key 前缀：代码侧统一 `obj/`，**容器部署下仍未统一**
//!
//! `COS_UPLOAD_PREFIX` 此前有两套实际取值（生产 `.env` 的 `drawings/` 与
//! `upload_file_for_owner` 硬编码的空串 ⇒ 桶根）。本轮把**代码侧**（`config.rs`
//! 缺省 / `.env.example` / 代码回退值三处）统一为 `obj/`。
//!
//! ⚠️ 2026-10-13 review 第 1 轮 MAJOR-2：**闭环没合上** ——
//! `docker-compose{,-local,-staging}.yml` 的 `${COS_UPLOAD_PREFIX:-drawings/}`
//! 共 6 处仍在**根仓**（backend-rust 仓外，本 PR 结构上无法触及），
//! **容器部署下前缀仍落 `drawings/`**；代码回退值 `obj/` 只在**不经 compose** 的
//! `cargo run` 场景生效。根仓那 6 处待单独提交。
//!
//! ⚠️ **不需要数据迁移、不需要跑 `cos_key_migrate`**：`t_part_file.object_key`
//! 存的是完整 key，读端点先 `head_object(db_object_key)` 优先命中 ⇒ 历史对象
//! 原地不动，只有新上传改落配置里的那个前缀。
//!
//! ### 刻意留在别的域的两处文件写入
//!
//! - `POST /api/v2/parts/batch` 的 **bindings 分支**（part 域）：端点本体是
//!   「批量建零件」，文件只是可选入参，不是一条独立的文件端点。
//! - `POST /api/v2/parts/batch-with-pdfs` / `POST /api/v2/assemblies` 的
//!   **multipart PDF**：只喂 `lopdf` 数页数，**不入库不上 COS**。
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
