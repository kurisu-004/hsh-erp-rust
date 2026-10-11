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
//! ## 端点守卫矩阵
//!
//! 报工台（前端路由 `/scan/*`）的路由守卫就是 `SHELF_ACCOUNT` —— 车间工控机用的
//! 就是这个角色账号；它的图纸预览打的是本域的**列表** `GET /api/v2/part-files` 与
//! **内容** `GET /api/v2/part-files/{id}/content`，这两条均对 `SHELF_ACCOUNT`
//! 放行（否则工控机点预览必然 403（40300））。
//!
//! | 端点 | 守卫 | 备注 |
//! |---|---|---|
//! | `GET /part-files`（列表） | 角色 M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF |
//! | `GET /part-files/{id}/content` | 角色 M / C / I / CNC / **SHELF** | 2026-10-10 放开 SHELF |
//! | `GET /part-files/{id}/url` | 权限点 `part.drawing.print`（默认矩阵 = M / C / I / CNC） | 2026-10-14 由角色白名单迁移；SHELF 默认不授，见下 |
//! | `POST /part-files`（上传） | 角色 M / C / CNC（`bind_uploaded_file` 只 M / C） | 未动 |
//! | `POST /part-files/{id}/delete` | 按 kind 派生（图纸类 M / C，G_CODE M / CNC） | 未动 |
//! | `GET /part-files/parts/{id}/files` | 角色 M / C / I / CNC | **handler 级**闸门，比 service 严 |
//! | `GET /part-files/parts/{id}/cnc-programs` | 角色 M / C / I / CNC | 同上 |
//! | `GET /part-files/parts/{id}/setup-sheets` | 角色 M / C / I / CNC | 同上 |
//!
//! **闸门分两层，不是巧合**：零件维度的三条嵌套列表端点各自在 handler 里先过一道
//! 4 角色（`handler.rs` 的 `list_part_files_for_part` / `list_part_cnc_programs` /
//! `list_part_setup_sheets`），再进 `PartFileService::list_files`（5 角色）。**取交集
//! ⇒ 4 角色**，SHELF_ACCOUNT 进不去。放开只发生在顶层 `GET /part-files` 一条上。
//! ⚠️ handler 这道闸门是本域的既有形状（`list_part_files_for_part` 一直有，另两条
//! 补齐后三条一致），**不是**为工控机账号新加的一层；前端对这三条的列表消费早已切走
//! （零件 / 装配件文件列表统一走 `GET /part-files?owner_id=…&kind=…`），现存消费方
//! 都在那些页面上、与 SHELF_ACCOUNT 无关。
//!
//! ### 为什么 `/url` 的默认矩阵不含 `SHELF_ACCOUNT`
//!
//! 2026-10-14 起 `/url` 的守卫是**权限点 `part.drawing.print`**（不再硬编码角色
//! 集合）：默认矩阵逐格等于迁移前的 4 角色，故 `SHELF_ACCOUNT` 默认仍进不来；
//! 管理员在角色面板把该点授给 `SHELF_ACCOUNT` 后本端点对它放行 —— 授权归面板管，
//! 代码只认点。下面是「默认不给」的取舍。
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
