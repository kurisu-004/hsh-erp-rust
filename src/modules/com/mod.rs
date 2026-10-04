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

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod applicant;
pub mod customer;
pub mod union_list;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/customers", customer::router())
        .nest("/applicants", applicant::router())
        .nest("/union-list", union_list::router())
}
