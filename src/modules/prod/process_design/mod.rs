//! prod::process_design 子模块 —— 制定工序页的「待制定工序零件」列表
//!
//! 2026-10-05 新增：前端「制定工序」页从 part 域
//! `GET /api/v2/parts?status=PENDING`（前端**不传 `limit`**，落 part 域缺省 50）
//! 切到本域 `GET /api/v2/prod/process-design/parts`（本域缺省 200、`clamp(1, 500)`，
//! 详见 [`service`] 模块内 `DEFAULT_LIMIT` 的口径说明）。
//!
//! ## 为什么要在 prod 域另起端点
//! part 域 `GET /parts` 在 service 层硬置 `part_only: true`
//! （`part/repo/sql/part_sql.rs` 的 `list_with_filters`），repo 据此在 SQL 里追加
//! `AND assembly_id IS NULL` 守卫，把**装配件的子零件全部排除**在结果集外
//! （`t_part.assembly_id` 是子件指向父装配件的逻辑 FK）。该守卫在 part 域是对的
//! —— 那个页面的 ALL / PART 两种模式里，装配件子件要由 service 层内存合并、不该
//! 独立成行 —— 但本页需要「所有还没定工序的零件」，子件当然也在内。
//!
//! 除该守卫外，两者的谓词（PENDING 闸门）与字段集（本页只需 7 个字段的最小集）
//! 也都不同，谓词/字段各写一套比在 part 域塞 `row_type` / `include_assemblies`
//! 之类的开关更干净。与 2026-10-01 的 `prod::programming` 是同一类改动：
//! **page 域从 part 域 `t_part` 读一份谓词、字段都不同的窄列表**。
//!
//! **part 域一行未改**：旧端点保留兼容（part 域 `GET /parts` 仍带
//! `part_only: true`），前端调用方切到本域。
//!
//! ## ⚠️ 本端点刻意**不加** `AND assembly_id IS NULL` 守卫
//! 这是本端点存在的全部理由：加上这道守卫，装配件的子零件就会重新从「制定工序」
//! 页消失（页内选零件 → 建工艺链 → 下发，子件同样需要工序）。**后人不要"好心"
//! 把它加回去** —— 若将来发现结果集混进了不该出现的行，请先确认那是「装配件主表行」
//! 还是「子件行」：子件行 `assembly_id` 有值，是本页的正常成员。
//!
//! 对应地，本端点**不提供** `row_type` / `include_assemblies` 这类开关参数
//! ——「没有那道守卫」不是某个取值下的行为，而是本端点固定不变的口径。
//!
//! ## 过滤谓词（page 级 `t_part` 列表）
//! 0. **软删闸门**：`p.deleted_at IS NULL`
//! 1. **状态闸门**：`p.status = 'PENDING'` —— 写死在 SQL 常量里，**不**作为 query
//!    参数暴露。`PENDING` 是本页的业务闸门（只有未开工的零件才需要制定工序），不是
//!    筛选旋钮；放开成一个 `status` 参数会让前端拼出「待制定工序页 + 查已完成零件」
//!    这种自相矛盾的请求。
//!
//! 详见 [`repo`](repo.rs) 模块 doc（含「字典序而非数值序」「NULLS LAST」两处易被
//! 当 bug 改的口径说明）。
//!
//! ## 模块结构（与 `prod::programming` 平行）
//! - `dto.rs` —— 入参（`ProcessDesignListQuery`，Query string）
//! - `vo.rs` —— 出参（`ProcessDesignPartItemOut` / `ProcessDesignPartListOut`）
//! - `repo.rs` —— SQL 真源（`ProcessDesignRepo` ZST + `list` / `count`，共用 `repo`
//!   模块内的 `FROM_SQL` 与 `WHERE_SKELETON` 两个私有常量）
//! - `service.rs` —— 业务逻辑（角色守卫 + limit/offset clamp + row→vo 投影）
//! - `handler.rs` —— HTTP 路由（只做参数提取 + `pool.acquire()` + `R::ok`）
//!
//! ## 事务 / WS 广播
//! 纯读端点：handler `pool.acquire()` 不开事务，**不发** WS 广播（无业务流转）。

use std::sync::Arc;

use axum::{Router, routing::get};

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/parts", get(handler::list_parts))
}
