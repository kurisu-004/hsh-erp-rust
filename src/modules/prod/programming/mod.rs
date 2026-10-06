//! prod::programming 子模块 —— 待编程一览（part 状态闸门 + 三规则并集口径）
//!
//! 前端「待编程一览」页的**唯一**数据源：`GET /api/v2/prod/programming/pending`。
//!
//! ## 为什么谓词要按权威列重写
//! 本端点的规则 3 读 `t_part_batch.current_process_id`（批次工序归属的唯一权威
//! 列，migration 004 确立），而非经 `t_part_batch.current_holder_id → t_shelf_process`
//! 的间接链路 —— 后者在 `t_process.is_cnc` 未正确维护 / 批次未上架时取不到工序。
//!
//! ## 过滤谓词（part 状态闸门 + 三规则并集，part 级去重）
//! 0. **状态闸门**：`p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')` —— 写在
//!    最外层，**约束全部三条规则**。因为 `t_part.process_chain_id` 从不清空，
//!    若规则2 不受状态约束，历史上挂过 CNC 链的 `COMPLETED` / `CANCELLED` /
//!    `DELIVERED` 工单会永久命中本页。
//! 1. `p.status = 'PROGRAMMING'` —— 兼容旧筛选（历史 PROGRAMMING 状态仍允许消化）
//! 2. 工单工艺链上存在 `is_cnc = TRUE` 的工序 step
//! 3. 工单存在批次，其 `current_process_id` 指向 `is_cnc = TRUE` 的工序
//!
//! 详见 [`repo`](repo.rs) 模块 doc（含 `next_process_id` 禁引用的坑）。
//!
//! ## 模块结构（与 `prod::batch` 平行）
//! - `dto.rs` —— 入参（`ProgrammingListQuery`，Query string）+ 两个私有反序列化
//!   兜底器；**本域入参 DTO 全在 `dto.rs`**，无 dashboard 那类「入参结构体定义在
//!   handler.rs」的例外
//! - `vo/` —— 出参隔离层：`vo/mod.rs`（域级契约 + 精确 re-export）+ `vo/pending.rs`
//!   （`ProgrammingItemOut` / `ProgrammingListOut`，仅 `Serialize`）
//! - `repo.rs` —— SQL 真源（`ProgrammingRepo` ZST + `list` / `count`，共用
//!   私有 `push_where`）；只收 service 规范化后的入参
//! - `service.rs` —— 业务逻辑（角色守卫 + limit/offset clamp + **排序白名单映射**
//!   + row→vo 投影）
//! - `handler.rs` —— HTTP 路由（只做参数提取 + `pool.acquire()` + `R::ok`）
//!
//! ## 分层口径（2026-10-07 对齐 dashboard 形态）
//! - **VO 不实现 `Deserialize`**：`vo/` 下的类型禁止出现在 axum extractor
//!   反序列化侧，只用于 service 组装 + handler `Json(R::ok(...))` 序列化；
//!   `vo/mod.rs` 做**精确 re-export**（逐个列出两个结构体，不 glob），本域类型不
//!   对外扩散。
//! - **排序白名单映射在 service 层**：`sort_by` / `sort_dir` 是外部字符串，经
//!   `service.rs::resolve_order_col` / `resolve_order_dir` 映射成列名 / 方向字面量
//!   才进 `repo.rs` 的 [`repo::ProgrammingFilters::order_col`] / `order_dir`；
//!   repo 收到的字段或已规范化（`keyword` / `serial_no`）、或以 `push_bind` 传参
//!   （`has_cnc_program` / `limit` / `offset`），拼进 SQL 文本的只有 `order_col` /
//!   `order_dir` 两个受控字面量（范式同 `prod::batch::service::list`）。非法
//!   `sort_by` 退化为计划交期、非法 `sort_dir` 退化为 `ASC`，均**不报错**。
//! - **`has_cnc_program` 真相源留 repo**：`repo.rs::G_CODE_EXISTS` 同一常量同时供
//!   list 的 SELECT 投影与 WHERE 三态过滤复用，**改一必须同步二**（义务登记见该
//!   常量 doc）。它与上面的排序白名单是两回事：白名单映射的是外部字符串→列名，
//!   必须外推到 service；而 `G_CODE_EXISTS` 是 SQL 片段，投影与过滤两处都在 repo
//!   内部，放 service 反而会把一段 SQL 拆成两半。
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
             t_part_file 等表，SQL 真源见 repo.rs），而不是 import 别人的 service / repo。",
        );
    }
}
