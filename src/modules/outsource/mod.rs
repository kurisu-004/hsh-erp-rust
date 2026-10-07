//! outsource 域（Phase 2 2026-09-13）
//!
//! 对应 Python myERP：
//! - api/v1/outsource_*.py
//! - service/outsource_*.py
//! - repository/outsource_*.py
//! - model/outsource.py
//! - schema/outsource.py
//! - statemachines/outsource_quote.py
//!
//! 2026-09-22 refactor（对齐 iam 事务分层范式）：
//! - `repo.rs` 拆为 `repo/{mod, sql}.rs`：胖 trait `OutsourceRepoTrait` + 3 ZST struct
//!   （`OutsourceCompanyRepo` / `OutsourceQuoteRepo` / `OutsourceShipmentRepo`）
//!   + 4 `NewXxx` insert builder；trait 直接 `impl for &mut PgConnection`。
//! - `service.rs`（1273 行超 1000 行上限）拆为 `service/{mod, company, quote, shipment}.rs`：
//!   `OutsourceService` 字段仅 `Arc<SnowflakeIdGenerator>`，impl 块分布在 4 子模块。
//! - `handler.rs` 17 端点保持 3 router 工厂（company_router / quote_router /
//!   shipment_router）；handler 三形态严格区分（读走 `pool.acquire()`，
//!   写走 `pool.begin() + tx.commit()`）。
//!
//! 2026-10-03 读侧补齐（20 端点 / 4 router 工厂）：
//! - 新增 4 个 list 端点：companies 的 `sent-parts`、quotes 的 `quotable-parts`、
//!   shipments 的 `in-flight`、独立顶层 `/outsource-sendable`。
//! - `vo` 增 2 文件（`quotable.rs` / `sendable.rs`），`service` 增 1 文件
//!   （`sendable.rs`），repo 增 2 ZST（`OutsourceQuotableRepo` / `OutsourceSendableRepo`）。
//! - 删除 part 域两个**错形状**的同义端点（返回通用 `PartListItem`，与前端字段
//!   需求不匹配）：`/parts/outsource-in-flight`、`/parts/outsource-sendable`
//!   （2026-10-03 硬切，无 alias；旧 URL 实际返回 **400** 而非 404 —— part 域
//!   `/{part_id}`（`Path<i64>`）catch-all 兜住任何未注册的 1 段静态路径，再由
//!   `Path` extractor 拒绝非数字段；成因与取舍见 `src/modules/part/mod.rs` 的
//!   模块 doc）。
//!
//! 2026-10-03 看板三件套（`/outsource-pool/*`，3 只读端点）：
//! - 新增第 5 个 router 工厂 `pool_router()`，独立顶层前缀 `/api/v2/outsource-pool`，
//!   形态照抄 `prod::pool`（`counts` / `state` / `{process_id}`）。
//! - `vo` 增 `pool.rs`，`service` 增 `pool.rs`，repo 增 ZST `OutsourcePoolRepo`
//!   （另给 `OutsourceSendableRepo` 增 `list_by_process`，两者共用同一份核心 SQL ——
//!   见 `repo/sql.rs::SENDABLE_INNER_X_SQL`）。
//!
//! 2026-10-09 看板收敛（`/outsource-queue/*`，2 只读端点）：
//! - `/outsource-pool/{counts,state,{process_id}}` **硬切到**
//!   `/outsource-queue/{snapshot,processes/{id}}`，**无 alias**。旧路径下打开一道工序
//!   的板要发 1（工序详情）+ M（每家公司一次 state）= M + 1 个 HTTP 请求；在途批次
//!   卡片内联进公司列后恒定 1 个请求。
//! - 新增 `board/` 子模块（`repo.rs` + `service.rs`，范本 `prod/queue/board/`）：
//!   固定 SQL 条数（snapshot 3 条 / detail 4 条），由
//!   `board/mod.rs::sql_count_guard_tests` 的源码级护栏钉住。
//! - `vo/pool.rs` 与 `service/pool.rs` 删除，出参合并进 `vo/queue.rs`；repo 侧
//!   `OutsourcePoolRepo` 整体删除（4 个方法在三条端点下线后全部无调用方），其 SQL
//!   按「去公司谓词」的口径搬进 `board/repo.rs`。`OutsourceRepoTrait` 相应瘦身 5 个
//!   方法（`pool_*` 4 + `sendable_list_by_process` 1）。
//! - ⚠️ `GET /outsource-sendable` 与 `/outsource-sendable/*` **暂留**（下一步随看板
//!   移动写端点接管删除）：它仍走 `OutsourceService` + `OutsourceRepoTrait`，与看板
//!   的 `board/` 聚合线并存但共用同一份候选侧谓词（`repo/sql.rs` 的
//!   `SENDABLE_INNER_X_SQL`），故两边的 `sendable_count` 与 `items` 仍逐行一致。

pub mod board;
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

/// 公司域路由（挂载点 `/outsource-companies`，见 `modules::v2_router`）。
pub fn company_router() -> Router<Arc<AppState>> {
    handler::company_router()
}

/// 报价域路由（挂载点 `/outsource-quotes`）。
pub fn quote_router() -> Router<Arc<AppState>> {
    handler::quote_router()
}

/// 发货记录域路由（挂载点 `/outsource-shipments`）。
pub fn shipment_router() -> Router<Arc<AppState>> {
    handler::shipment_router()
}

/// 可发送外协一览路由（挂载点 `/outsource-sendable`）。
///
/// 2026-10-03 新增。独立顶层前缀：可发送判定横跨 company / quote / batch 三域，
/// 不属于任何单一域的子资源。
///
/// ⚠️ **随看板移动写端点接管删除**（当前仍在线）：`GET /outsource-queue/processes/{id}`
/// 的候选列已经是不分页的同一批行，本端点是它的一个分页子集。
pub fn sendable_router() -> Router<Arc<AppState>> {
    handler::sendable_router()
}

/// 外协看板路由（挂载点 `/outsource-queue`，见 `modules::v2_router`）。
///
/// 2026-10-09 更名（旧挂载点 `/outsource-pool`，形态与前一条一同下线）。改名的
/// 理由与 `prod::worker_pool → prod::queue` 同源：`pool` 只覆盖了「候选池」一块，
/// 而本端点返回的是「候选 + 公司列 + 在途批次」的整块看板。
pub fn queue_router() -> Router<Arc<AppState>> {
    handler::queue_router()
}
