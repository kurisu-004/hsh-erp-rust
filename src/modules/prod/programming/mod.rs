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
//! **part 域一行未改**：旧端点保留兼容，新增前端调用方一律走本域。
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
