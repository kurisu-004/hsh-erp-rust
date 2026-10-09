//! part_file 域
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
//! ## 角色矩阵（2026-10-10 新增 `Role::ShelfAccount` 的两条只读端点）
//!
//! 报工台（前端路由 `/scan/*`）的路由守卫就是 `SHELF_ACCOUNT` —— 车间工控机用的
//! 就是这个角色账号；它的图纸预览打的是本域的**列表** `GET /api/v2/part-files` 与
//! **内容** `GET /api/v2/part-files/{id}/content`。这两条原本只放 4 角色
//! （Manager / Clerk / Inspector / CncProgrammer）⇒ 工控机点预览必然 403（40300）。
//!
//! | 端点 | 角色集合 | 备注 |
//! |---|---|---|
//! | `GET /part-files`（列表） | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF |
//! | `GET /part-files/{id}/content` | M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF |
//! | `GET /part-files/{id}/url` | M / C / I / CNC | **刻意不含 SHELF**，见下 |
//! | `POST /part-files`（上传） | M / C / CNC（`bind_uploaded_file` 只 M / C） | 未动 |
//! | `POST /part-files/{id}/delete` | 按 kind 派生（图纸类 M / C，G_CODE M / CNC） | 未动 |
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
//! `owner_id`），`part_file.req` 允许客户端任意指定 `owner_id`。放开 SHELF_ACCOUNT 后，
//! **工控机账号能拉取任意 `owner_id` 的文件**，没有货架级 / 工单级的收窄 —— 本域
//! 的权限模型止步于「角色」，不含 `CurrentUser.shelf_ids` 那一层范围判定。
//! 这是为「工控机看图纸」这个刚需做的显式取舍；真要收窄需要先给 `t_part_file` 加
//! owner 侧的范围索引（哪张单 / 哪个货架），属独立立项，不在本次范围。
//!
//! 本域不在 `docs/api/` 的整域覆盖清单内（该目录只覆盖部分域，判据见
//! `CLAUDE.md` 的「docs/api/ 目录约定」一节；`ls docs/api/` 复核），故角色集合的
//! 真源就是本模块 doc + 各 service / handler 端点 doc。
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

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
