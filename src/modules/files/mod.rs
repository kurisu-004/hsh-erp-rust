//! files 域 —— **文件资源容器域**
//!
//! 与 `iam`（含嵌套子模块 `iam::shelf`）、`com`、`prod` 同一形态：一个容器域，
//! 底下挂若干嵌套子模块，各自带自己的 repo / service / 路由表。
//!
//! ## 端点表（7 条，全部挂在 `/api/v2/files/objects`，前缀由
//! `handler::router()` 的 `.nest("/objects", …)` 声明）
//!
//! | 方法 | URL | 角色 | 归属子模块 |
//! |---|---|---|---|
//! | `GET` | `/api/v2/files/objects` | M / C / I / CNC / **SHELF** | `part_file`（列表） |
//! | `POST` | `/api/v2/files/objects/upload-intents` | M / C / CNC（**`CUSTOMER`：仅 M**） | `part_file`（签发预签 PUT URL） |
//! | `POST` | `/api/v2/files/objects/confirm` | M / C（G_CODE/SETUP_SHEET：M / CNC；**`CUSTOMER`：仅 M**） | `part_file`（直传登记 + CNC 配对） |
//! | `GET` | `/api/v2/files/objects/{id}/url` | 权限点 `part.drawing.print` | `part_file` |
//! | `GET` | `/api/v2/files/objects/{id}/content` | M / C / I / CNC / **SHELF** | `part_file` |
//! | `POST` | `/api/v2/files/objects/{id}/delete` | 按 kind 派生 | `part_file` |
//! | `GET` | `/api/v2/files/objects/{id}/preview` | M / C / I / CNC / **SHELF** | `part_file::preview`（COS CI 预览，支持可选 query `?size=view\|thumb\|full`） |
//!
//! ### ⚠️ 全仓**唯一**的收文件链路（2026-10-15）
//!
//! ```text
//! 用户点提交 → ① 业务提交（纯 JSON）拿 owner_id
//!          → ② POST /files/objects/upload-intents  （签发预签 PUT URL）
//!          → ③ 前端 XHR PUT 直传 COS（字节不过 rust 进程）
//!          → ④ POST /files/objects/confirm        （head 校验 + INSERT，可选 CNC 配对）
//! ```
//!
//! 同 owner + kind + 同 `content_sha256` 已有**活跃行**时，② 直接回
//! `already_exists: true` + `existing_file`（秒传），前端跳过 ③④。
//!
//! ⚠️ **`/preview` 与 `/url` 的角色集合看似矛盾，实为有意**：`/url` 回的是 COS 预签
//! 直链（`COS_PRESIGN_EXPIRE` 下发，缺省 1 小时），**刻意排除** SHELF；
//! `/preview` 回的是 `previews/` 前缀下的**永久公开读**直链，**放开** SHELF。
//! 后者比前者**更宽**却仍选择放开，是用户拍板的安全取舍反转（目的是让报工台工控机看
//! 图纸预览），完整理由 / 风险 / 补偿见 `part_file/preview/mod.rs` 的「安全取舍反转」
//! 一节。**不要**「顺手把两者对齐」。
//!
//! ### ⚠️ 列表端点**不带尾斜杠**（`GET /api/v2/files/objects`，不是 `.../objects/`）
//!
//! axum 0.8 的 `nest` 把内层 `.route("/", …)` 注册成 `<prefix>` 本身，故
//! `GET /api/v2/files/objects/` 是 **404**。端点映射表里写的
//! 「`GET /part-files/` → `GET /files/objects/`」中的尾斜杠来自路由注册串
//! `.route("/", …)`，**不是 wire 上的 URL**。前端照旧用**无尾斜杠**即可；
//! 该形态由 `tests/files/part_file.rs::old_part_files_prefix_returns_404` 里的
//! 正向对照（200）+ 尾斜杠对照（404）双向钉死。
//!
//! 各端点的逐字段契约、角色矩阵理由、移除记录见 `part_file/mod.rs` 与
//! `part_file/handler.rs` 的模块 doc。
//!
//! ⚠️ **本表的计数真源是两张 `ROUTES` 常量**：`part_file::handler::ROUTES`（7 条，
//! 即本表全部端点）与 `handler::ROUTES`（0 条 —— 容器层只 nest、无自有端点）。
//! 两张表都用**同一个**比对器（`route_guard::routes_in_router_source`）逐条核对
//! 各自的 `router()` 源码。⚠️ 加端点忘改本表**不会红**（本表是人读的），但忘了改
//! 对应的 `ROUTES` 会当场红。
//!
//! ## ⚠️ 硬切记录
//!
//! ### 2026-10-15 上传链路统一（本轮）
//!
//! | 旧 URL | 处置 | 说明 |
//! |---|---|---|
//! | `POST /api/v2/files/sts-tmp-keys` | **删** | python STS 凭证转发；预签 PUT 改由 `POST /files/objects/upload-intents` 在本仓签发（OpenDAL SigV4）。⚠️ python 侧同名端点 `POST /api/v1/files/sts-tmp-keys` 随之失去唯一调用方，待后续在 backend-python 仓清理 |
//! | `POST /api/v2/files/objects`（multipart 代理） | **删** | 字节不再经 rust 进程；由 upload-intents + 直传 + confirm 承接 |
//! | `POST /api/v2/files/cnc/pairs` | **删** | CNC 配对由 confirm 的 `pair_with_file_id` 承接 |
//!
//! 全部**无 alias、旧路径 404**。
//!
//! ### 2026-10-13 域重组（历史）
//!
//! 本域由独立 `part_file` 域 + `cnc_program` 域收编而成：
//!
//! | 旧 URL | 新 URL | 处置 |
//! |---|---|---|
//! | `GET /api/v2/part-files` | `GET /api/v2/files/objects` | 改名 |
//! | `GET /api/v2/part-files/{id}/url` | `GET /api/v2/files/objects/{id}/url` | 改名 |
//! | `GET /api/v2/part-files/{id}/content` | `GET /api/v2/files/objects/{id}/content` | 改名 |
//! | `POST /api/v2/part-files/{id}/delete` | `POST /api/v2/files/objects/{id}/delete` | 改名 |
//! | `POST /api/v2/parts/{part_id}/upload-drawing` | `POST /api/v2/files/objects` | 四合一（该入口已随上表下线） |
//! | `POST /api/v2/parts/{part_id}/upload-3d-model` | `POST /api/v2/files/objects` | 四合一（同上） |
//! | `POST /api/v2/assemblies/{assembly_id}/files` | `POST /api/v2/files/objects` | 四合一（同上） |
//! | `POST /api/v2/parts/{part_id}/files/confirm` | `POST /api/v2/files/objects/confirm` | 搬家 + body 加 `owner_kind` / `owner_id` |
//! | `POST /api/v2/cnc-programs/pairs` | `POST /api/v2/files/cnc/pairs` | 改名（已随上表下线） |
//! | `GET /api/v2/assemblies/{assembly_id}/files` | — | 删（0 调用方） |
//!
//! 同批另删 12 条零调用方端点（part_file 域 8 条 + cnc 域 4 条）。
//! 旧路径**一律 404、无 alias**，守卫用例见 `tests/files/part_file.rs`。
//!
//! ### COS 对象 key 前缀：代码侧统一 `obj/`，**容器部署下仍未统一**
//!
//! `COS_UPLOAD_PREFIX` 的缺省值（`config.rs` / `.env.example`）统一为 `obj/`，写入路径
//! 由 `util::cos_key::build_cas_key` 派生。
//!
//! ⚠️ **容器部署下前缀仍可能落 `drawings/`**：`docker-compose{,-local,-staging}.yml`
//! 的 `${COS_UPLOAD_PREFIX:-drawings/}` 共 6 处仍在**根仓**（backend-rust 仓外，
//! 本仓结构上无法触及），代码缺省值只在**不经 compose** 的 `cargo run` 场景生效。
//!
//! ⚠️ **不需要数据迁移**：`t_part_file.object_key` 存的是完整 key，读端点先
//! `head_object(db_object_key)` 优先命中 ⇒ 历史对象原地不动，只有新上传改落配置里
//! 的那个前缀。
//!
//! ### 刻意留在别的域的两处文件写入
//!
//! - `POST /api/v2/parts/batch`（part 域）**已回归纯建单**：原先的 `drawing_file` /
//!   `model3d_file` bindings 入参与配套的 head/copy 分支随统一链路删除。
//! - `POST /api/v2/assemblies` 的 **multipart PDF**：只喂 `lopdf` 数页数，
//!   **不入库不上 COS**。

mod handler;

pub mod part_file;

/// 「`router()` 源码 ↔ `ROUTES` 常量」比对器的共用实现（**仅 `#[cfg(test)]`**，
/// 不进 lib 产物 ⇒ 只有 `src/` 内的单元测试能用）。
///
/// 三个 router 工厂（`handler` / `part_file::handler` / **域外的
/// `print_template::handler`**）的 `mod tests` 都调它，见该文件模块 doc。
///
/// ⚠️ 2026-10-16：`mod` 改 `pub(crate) mod` —— `print_template` 域（**不属于
/// `files`**）的路由清单比对器也复用这一份实现。函数本身早已是 `pub(crate)`，
/// 但外层模块是私有的 ⇒ 跨模块够不着。**刻意不复制第二份解析器**：两份同名同款
/// 的「从源码抠路由表」逻辑必然漂移，而漂移的表现是「守卫静默失效」
/// （写成 `contains("GET /resolve")` 硬断言那种，加了第 6 条端点不会红）。
#[cfg(test)]
pub(crate) mod route_guard;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// files 域路由工厂（转发式：路由表在 `handler::router()`）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
