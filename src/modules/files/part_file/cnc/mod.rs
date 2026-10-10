//! `part_file::cnc` 子模块 —— CNC 配对（**组合视图，无自有存储**）
//!
//! 对应 Python myERP：
//! - api/v1/cnc_program.py
//! - service/cnc_program_service.py
//!
//! ## 它是什么
//! 一条「CNC 程序」在本仓不是独立实体，而是**两条 `t_part_file` 行的组合视图**：
//! `kind='G_CODE'` + `kind='SETUP_SHEET'`，两行的 `paired_file_id` 互相指向对方。
//! 本子模块只提供「一次提交一对」这一个动作，读侧一律走 `part_file` 的对象端点。
//!
//! ## 端点表（1 条）
//!
//! | 方法 | URL | 角色 | 说明 |
//! |---|---|---|---|
//! | `POST` | `/api/v2/files/cnc/pairs` | M / CNC | multipart：`data` JSON + `g_code` + `setup_sheet` |
//!
//! 2026-10-13：**自独立 `cnc_program` 域收编进 `files` 域**（作为 `files::part_file::cnc`
//! 子模块）。URL 由 `POST /api/v2/cnc-programs/pairs` **硬切**到
//! `POST /api/v2/files/cnc/pairs`，**无 alias，旧前缀 404**。
//!
//! ## 2026-10-13 同批下线的东西（移除记录，防后人重建）
//!
//! - **3 条 alias 端点**（`GET /{id}/download-url` / `GET /{id}/content` /
//!   `POST /{id}/delete`）：零前端调用方（前端统一走 `/files/objects/{id}/*`）。
//!   它们本来只是 handler 直转 `state.part_file_service` 的薄壳，删掉不丢任何能力。
//! - **`GET /cnc-programs/parts/{part_id}`（配对列表）**：零前端调用方，前端 CNC 配对
//!   分组由 `GET /files/objects?owner_kind=PART&owner_id=…&kind=G_CODE` 驱动
//!   （列表项带 `paired_file_id`，见 `parent::vo::PartFileOut`）。连同
//!   `CncProgramService::list_pairs_for_part` 与其 repo SQL 一并删除。
//! - **part_file 侧那条重复的 `POST /part-files/parts/{part_id}/cnc-pair`**：它与本
//!   端点调的是同一个 `CncProgramService::upload_cnc_pair`，只是 multipart 字段名
//!   略有差别（`setup_sheet` vs 同名字段）。零前端调用方，随本批下线。
//! - **整个 `cnc/repo/` 目录**（原 241 + 66 行）：`CncProgramRepoTrait` 的 6 个方法里
//!   5 个在 `parent::repo::PartFileRepoTrait` 中已有一份等价实现（其中
//!   `insert_setup_sheet_with_paired` / `set_paired_file_id` 的 SQL 字符串原本在两个
//!   trait impl 内**逐字重复**），本子模块改为直接用父模块 repo，双向 handler/service
//!   循环随之断开。
//!
//! ## 结构
//! - `service.rs`：`CncProgramService` 仍是带字段结构（`Arc<SnowflakeIdGenerator>` +
//!   `Arc<dyn CosClient>`），方法签名泛型参数由 `CncProgramRepoTrait` 换成
//!   `parent::repo::PartFileRepoTrait`（`<R: PartFileRepoTrait>(&self, mut repo: R, ...)`）。
//! - 入参：**没有 `dto.rs`** —— handler 在 `handler.rs` 里 inline 了
//!   `PairUploadData`（multipart `data` JSON 字段的反序列化目标）直接用。原
//!   `dto.rs` 的 `CncPairUploadRequest` **全仓零引用**（master 继承的死类型），随本批
//!   2026-10-13 删除，别再按旧注释「dto.rs：仅入参」重建。
//! - `vo/cnc_pair.rs`：仅出参（`CncFileRef` / `CncPairOut`）。
//! - ⚠️ 2026-10-13：`model.rs`（7 行占位，全文是「本文件目前为占位」）随本批删除，
//!   本子模块没有自己的表行模型。
pub mod handler;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

/// CNC 配对子路由，**挂在 `/cnc` 下**（见 `parent::handler::router()` 的
/// `.nest("/cnc", …)` → 完整路径 `/api/v2/files/cnc/*`）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
