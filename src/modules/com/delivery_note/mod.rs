//! com::delivery_note 子模块（送货单 + 送货分组 + 司机候选）
//!
//! 2026-10-08 自 `src/modules/delivery_note/` 平移进 `com` 聚合（与
//! 2026-09-19 com 聚合、2026-10-02 shelf_process 硬切同一先例）。URL **硬切无
//! alias**：旧 `/api/v2/com/delivery/note/*` 与 `/api/v2/com/delivery/group/*` 一律 404。
//!
//! ## 路由挂载（三层 nest）
//! ```text
//! v2_router  .nest("/com",   com::router())
//! com        .nest("/delivery", delivery_note::router())
//! 本模块      .nest("/note",     handler::note_router())    送货单本体
//!            .nest("/group",    handler::group_router())   送货分组 CRUD
//!            .nest("/drivers",  handler::drivers_router()) 候选送货司机一览
//! ```
//! ⇒ 送货单是 `/api/v2/com/delivery/note/*`、送货分组是
//! `/api/v2/com/delivery/group/*`、司机候选是 `/api/v2/com/delivery/drivers`；送货单
//! 与送货分组的**权威路由清单**分别是 [`handler::ROUTES`] / [`handler::GROUP_ROUTES`]
//! （单测断言它们与对应 `router()` 源码逐条一致）。
//!
//! ## 子模块
//! - `dto` —— 仅 `Deserialize` 入参（出参全在 `vo`）
//! - `vo` —— 仅 `Serialize` 出参（按端点语义分文件）
//! - `model` —— `t_delivery_group*` / `t_delivery_note*` 行结构 + 域枚举
//! - `repo` —— SQL 真源 `sql.rs` + 胖 trait `DeliveryNoteRepoTrait`
//! - `service` —— 业务逻辑（按端点语义拆子模块）
//! - `statemachine` —— `DeliveryNoteStatus` 迁移表入口
//! - `handler` —— HTTP 层 + `router()` 挂载入口
//!
//! 整域契约见 `docs/api/delivery_note.md`（八节骨架）；`docs/api/` 下**仅此一份**
//! 覆盖本域，其余信息在代码注释里。
//!
//! ## 对应 Python myERP（历史映射，仅供溯源）
//! - `api/v1/delivery_note.py` → `handler/`
//! - `service/delivery_note.py` → `service/`
//! - `repository/delivery_note.py` → `repo/`
//! - `model/delivery_note.py` → `model.rs`
//! - `schema/delivery_note.py` → `dto.rs`
//! - `statemachines/delivery_note.py` → `statemachine.rs`
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;
pub mod vo;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// 本模块总路由（挂载点见 `com::mod.rs` 的 `.nest("/delivery", ...)`）。
///
/// 两个子 nest 的分工：`note` = 送货单本体（扫码入单 / 生命周期 / 编辑），
/// `group` = 送货分组 CRUD。`t_delivery_group` / `t_delivery_group_member` 两张表
/// 仍在使用（分单按 L2 归属展示），故 `/group` 段保留。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/note", handler::note_router())
        .nest("/group", handler::group_router())
        .nest("/drivers", handler::drivers_router())
}
