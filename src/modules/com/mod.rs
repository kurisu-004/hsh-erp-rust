//! com 域（Customer Order Management）
//!
//! 2026-09-19 新增 com 模块聚合：把 customer + applicant 两个域代码组织上归位到 com 下，
//! URL 一并迁移到 `/api/v2/com/*`。order 域后续再讨论，本任务不涉及。
//!
//! 路由风格保持不变：customer/applicant 各自定义 5 个标准 CRUD 端点。
//!
//! 2026-09-29 新增 `union_list` 子模块：跨 `t_part` + `t_assembly` 合并视图端点
//! `GET /api/v2/com/union-list`，承担原 part 域 `row_type` 矩阵（ALL / PART /
//! ASSEMBLY） + 修分页 bug（plan §1-3）。part 域回退到纯 `t_part WHERE
//! assembly_id IS NULL` 查询（不再处理装配件）。
//!
//! 2026-10-05 该端点加第四态 `row_type=PART_FLAT`（合计四态）：仅 `t_part`、含
//! 装配件子件，与 dashboard 交期分桶柱状图的 `t_part` 行口径一致（下钻列表用），
//! 装配件父行（`t_assembly`）不计入不展示。
//!
//! 2026-10-08 新增 `delivery_note` 子模块：送货单 + 送货分组自顶层
//! `delivery_note` 域平移进来，URL **硬切无 alias**（旧 `/api/v2/delivery-notes`
//! 与 `/api/v2/delivery-groups` 一律 404）—— 与 2026-09-19 com 聚合、
//! 2026-10-02 shelf_process 硬切同一先例。
//!
//! 送货单的多个前缀收敛成**一个** nest：`/delivery` 下再分 `note`（送货单本体）
//! 与 `group`（送货分组）。分组表 `t_delivery_group` / `t_delivery_group_member`
//! 仍在使用（按 L2 归属分单展示），故 `/group` 段保留。整域契约见
//! `docs/api/delivery_note.md`。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod applicant;
pub mod customer;
pub mod delivery_note;
pub mod union_list;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/customers", customer::router())
        .nest("/applicants", applicant::router())
        .nest("/union-list", union_list::router())
        // 2026-10-08 送货单 + 送货分组（自顶层 delivery_note 域平移，URL 硬切无 alias）。
        // 内部再分 `/note` 与 `/group` 两个子 nest，见 `delivery_note::router()`。
        .nest("/delivery", delivery_note::router())
}
