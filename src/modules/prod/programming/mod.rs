//! prod::programming 子模块 —— 待编程一览（part 状态闸门 + 三规则并集口径）
//!
//! 2026-10-01 新增：前端「待编程一览」页从 part 域
//! `GET /api/v2/parts/pending-programming` 切到本域
//! `GET /api/v2/prod/programming/pending`。
//!
//! ## 为什么要在 prod 域另起一个端点
//! part 域旧端点的谓词在开发库恒返空：规则 B 依赖「批次所在货架 → 工序」链路，
//! 而 `t_process.is_cnc` 全 false、`t_process_chain_step` 0 行 → 无任何工单命中。
//! 同时该端点读的是 `t_part_batch.current_holder_id → t_shelf_process` 这条
//! **间接**链路，而 migration 004 起 `t_part_batch.current_process_id` 才是批次
//! 工序归属的**唯一权威依据**。新端点按权威列重写规则 3，语义更准、命中更稳。
//!
//! **part 域一行未改**：旧端点保留兼容（仅 `docs/api/parts/lifecycle.md` 追加
//! 弃用说明），新增前端调用方一律走本域。
//!
//! ## 过滤谓词（part 状态闸门 + 三规则并集，part 级去重）
//! 0. **状态闸门**：`p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')` —— 写在
//!    最外层，**约束全部三条规则**（2026-10-01 review 第 1 轮 A 项拍板）。因为
//!    `t_part.process_chain_id` 从不清空，若规则2 不受状态约束，历史上挂过 CNC 链的
//!    `COMPLETED` / `CANCELLED` / `DELIVERED` 工单会永久命中本页。旧 part 域端点
//!    本来就有这条闸门（`tests/part/lifecycle.rs::
//!    list_pending_programming_excludes_completed_or_cancelled` 锁住），新端点不得更宽。
//! 1. `p.status = 'PROGRAMMING'` —— 兼容旧筛选（历史 PROGRAMMING 状态仍允许消化）
//! 2. 工单工艺链上存在 `is_cnc = TRUE` 的工序 step
//! 3. 工单存在批次，其 `current_process_id` 指向 `is_cnc = TRUE` 的工序
//!
//! 详见 [`repo`](repo.rs) 模块 doc（含 `next_process_id` 禁引用的坑）。
//!
//! ## 模块结构（与 `prod::batch` 平行）
//! - `dto.rs` —— 入参（`ProgrammingListQuery`，Query string）
//! - `vo.rs` —— 出参（`ProgrammingItemOut` / `ProgrammingListOut`）
//! - `repo.rs` —— SQL 真源（`ProgrammingRepo` ZST + `list` / `count`，共用
//!   私有 `push_where`）
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
    Router::new().route("/pending", get(handler::list_pending))
}

#[cfg(test)]
mod tests {
    //! 域隔离护栏：把「待编程域不依赖其它域」从口头约定变成 CI 强制。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
    //! [`crate::shared::domain_guard`]，本域只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    /// 待编程域只允许 `crate::` 下的 `auth` / `infra` / `shared` / `state`
    /// 与本域自身；代码区里出现任何其它域的路径即失败（`prod` 下的兄弟域同样是
    /// 别的域，如 `prod::batch`）。
    #[test]
    fn programming_domain_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "prod::programming",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/prod/programming"),
            "需要别的域的数据时，正确做法是像 statistics / admin 那样在本域 SQL 里只读聚合\
             （待编程口径要读的 t_part / t_process / t_process_chain_step / t_part_batch / \
             t_part_file 等表，SQL 真源见 [`repo`](repo.rs)），而不是 import 别人的 service / repo。",
        );
    }
}
