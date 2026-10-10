//! `files::part_file` 子模块 —— `t_part_file` 表的整域
//!
//! 对应 Python myERP：
//! - api/v1/part_file.py
//! - service/part_file_service.py
//! - repository/part_file_repository.py
//! - model/part_file.py
//! - schema/part_file.py
//!
//! 2026-09-14 Phase 3：补齐 service / handler / DTO；polymorphic owner（PART / ASSEMBLY）；
//! SHA-256 CAS 去重 + COS 预签下载 URL。
//!
//! 2026-09-22 对齐 iam 范式：
//! - `repo.rs` → `repo/{mod, sql}.rs`：ZST `PartFileRepo` + 胖 trait `PartFileRepoTrait`。
//!   `PartFileRepo` 静态方法签名零 diff（跨模块调用方零修改）；`PartFileRepoTrait` 含
//!   12 方法（sql 6 + 跨域 owner 校验 2 + 软删 2 + cnc_pair 2），对 `&mut PgConnection`
//!   直接实现。
//! - `service.rs`：`PartFileService` 成为带字段结构（`Arc<SnowflakeIdGenerator>` +
//!   `Arc<dyn CosClient>`），方法签名 `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`
//!   by-value；handler 借 `&mut *tx` / `&mut *conn` 喂给 trait。
//!
//! ## ⚠️ 2026-10-13 自独立 `part_file` 域收编进 `files` 域
//!
//! - 源码 `src/modules/part_file/` → `src/modules/files/part_file/`（`git mv`，内容随迁移改动）。
//! - URL 由 `/api/v2/part-files/*` **硬切**为 `/api/v2/files/objects/*`，
//!   **无 alias，旧前缀 404**。原因是 URL 语义从「零件文件」改为「文件对象」——
//!   `t_part_file` 的 owner 本来就是多态的（`owner_kind ∈ {PART, ASSEMBLY}`），
//!   `/part-files` 这个名字与实际承载面不符。
//! - 端点 12 条 → **4 条**（详见 `handler.rs` 模块 doc 的移除记录表）；
//!   写路径（HTTP 上传）不在本子模块，走 STS 直传 + part 域
//!   `POST /api/v2/parts/{part_id}/files/confirm`。
//!
//! ## 角色矩阵（4 条存活端点；2026-10-10 新增 `Role::ShelfAccount` 的两条只读端点）
//!
//! 报工台（前端路由 `/scan/*`）的路由守卫就是 `SHELF_ACCOUNT` —— 车间工控机用的
//! 就是这个角色账号；它的图纸预览打的是本子模块的**列表** `GET /api/v2/files/objects`
//! 与**内容** `GET /api/v2/files/objects/{id}/content`。这两条原本只放 4 角色
//! （Manager / Clerk / Inspector / CncProgrammer）⇒ 工控机点预览必然 403（40300）。
//!
//! | 端点 | 角色集合 | 备注 |
//! |---|---|---|
//! | `GET /files/objects`（列表） | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF |
//! | `GET /files/objects/{id}/content` | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF |
//! | `GET /files/objects/{id}/url` | M / C / I / CNC | **刻意不含 SHELF**，见下 |
//! | `POST /files/objects/{id}/delete` | 按 kind 派生（图纸类 M / C，G_CODE M / CNC） | 未动 |
//!
//! ### 2026-10-13：零件维度三条嵌套列表端点下线 ⇒ 不再有「两层闸门取交集」
//!
//! 原表里另有三条 `GET /part-files/parts/{id}/{files,cnc-programs,setup-sheets}`
//! （handler 级 4 角色）比 service 的 `list_files`（5 角色）严一档，取交集 ⇒ 4 角色。
//! **这三条已随零调用方端点清理整体下线**（见 `handler.rs` 移除记录），故本域现在
//! 不再有「handler 级闸门与 service 白名单分叉」这件事：每个端点只有一层闸门，且
//! 那层闸门就在 service 里。角色矩阵回到「一个端点一行、集合唯一」的状态。
//!
//! ### 为什么 `/url` 不跟着放开
//!
//! `get_file_with_url` 回的是 **COS 预签直链**（`presigned_get_url`，
//! `url_expires_in_seconds = 3600`）：链接一旦到手就能**脱离本后端**直接 GET，
//! 也能整条外发给任何人 ⇒ 它等价于「把这 1 小时的 COS 读权限发出去」，而 RBAC
//! 在签发之后就不再参与。`get_file_content` 不同：字节流由本后端代理，每次访问都
//! 重新过一次 RBAC。工控机预览需要的是「看得到图纸字节」，content 已足够 ⇒ 放开
//! content 即达成目标，`/url` 无需、也不应一并放开。
//!
//! ### ⚠️ 已知安全面（**产品已拍板的取舍，不是待办**）
//!
//! `t_part_file` 的 owner 是**多态**的（`owner_kind ∈ {PART, ASSEMBLY}` +
//! `owner_id`），`GET /files/objects` 允许客户端任意指定 `owner_id` 过滤。放开
//! SHELF_ACCOUNT 后，**工控机账号能拉取任意 `owner_id` 的文件**，没有货架级 /
//! 工单级的收窄 —— 本域的权限模型止步于「角色」，不含 `CurrentUser.shelf_ids` 那一层
//! 范围判定。这是为「工控机看图纸」这个刚需做的显式取舍；真要收窄需要先给
//! `t_part_file` 加 owner 侧的范围索引（哪张单 / 哪个货架），属独立立项。
//!
//! 本域不在 `docs/api/` 的整域覆盖清单内（该目录只覆盖部分域，判据见
//! `CLAUDE.md` 的「docs/api/ 目录约定」一节；`ls docs/api/` 复核，无 `files.md` /
//! `part_file.md`），故角色集合与端点清单的真源就是本模块 doc +
//! `parent::mod.rs` 模块 doc + 各 service / handler 端点 doc。
pub mod cnc;
pub mod dto;
pub mod handler;
pub mod model;
pub mod policy;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

/// 本子模块路由工厂（转发式：路由表在 `handler::router()`）。
///
/// ⚠️ **挂载点不在 `modules::v2_router()`**：本域作为 `files` 域的嵌套子模块，
/// 由 `files::handler::router()` 的 `.nest("/objects", …)` 挂载 ⇒ 完整路径
/// `/api/v2/files/objects/*`。这一处挂载登记在 `src/modules/mod.rs::v2_router()` 上方
/// `.nest("/files", files::router())` 的注释里。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
