//! `serial` 域 HTTP handler：1 个端点 + 路由表（**2026-10-19 新建**）
//!
//! 端点（挂在 `/api/v2/serials`，由 [`router`] 桥接）：
//! - `POST /release-terminal` —— 一键释放「已完成 / 已取消且仍占着序列号」的工单，
//!   入参 `{"dry_run": bool}`，出参计数 + 明细。
//!
//! ## 角色守卫（**下沉在 handler 第一行**，仓库惯例）
//!
//! [`release_terminal_serials`] 自己不在 handler 判、而在 service 第一行判
//! （`service::release_terminal` 的守卫 ①）。这与 `print_template` 那两档守卫写在
//! handler 第一行的形态不同，原因见 `service` 模块 doc：角色闸必须**早于**全表扫，
//! 而 service 是唯一能保证「候选 SELECT 之前」的位置。
//!
//! ## ⚠️ 事务边界：**dry_run 不开事务**
//!
//! | 分支 | 事务 | 理由 |
//! |---|---|---|
//! | `dry_run = true` | **不开**，`pool.acquire()` | 纯只读（4 条 SELECT）。为一个预览请求开事务会让 PG 事务里的只读快照把连接按事务时长占住，且没有任何写要提交 |
//! | `dry_run = false` | `pool.begin()` → service → `tx.commit()` | 必须有：整批候选的清列 + 子件归档事件必须**全成功或全回滚**。半途失败留下一部分清了、一部分没清，比一行没清更难排查 |
//!
//! ## ⚠️ WS 广播：**commit 之后、一条汇总事件**
//!
//! 实跑成功后广播**一条** `TERMINAL_SERIALS_RELEASED`，payload 只带两个计数。
//! **不逐行广播**：整表扫的形态下逐行就是几千条噪音帧，把 dashboard 的增量通道淹掉。
//! dry_run **不广播**（零写入 ⇒ 零事件）。
//!
//! ## ⚠️ 本事件**驱动** dashboard（不是「零驱动作用」）
//!
//! dashboard 的出参多处带 `serial_no`（`WorkerHeldBatch` / `SystemDeliveryOrder` /
//! `DeliveryOrderDetail`），释放后这些切片上的序列号即陈旧 ⇒ 前端必须靠事件失效重取。
//! 真正会被本端点改到内容的是 `in_process` 切片：那条 SQL 只卡批次状态
//! `IN_PROCESS`、**不卡 `t_part.status`**，所以「工单已终态、批次仍压在工人手上」的
//! 残留行本就带序列号进大屏，而它正是本端点的释放对象。交期面板与下钻抽屉也 SELECT
//! `serial_no`，但谓词卡的是 6 个**非终态**，本端点动不到；`upcoming-delivery` 分桶
//! 只出计数、不含 `serial_no`。
//!
//! 前端已把 `TERMINAL_SERIALS_RELEASED` 加进 `AFFECTS_DASHBOARD`（前端仓 commit
//! `7cdd7de0`）。⚠️ 那份白名单是**前端**的资产、后端无感知：**不得把「前端已接线」
//! 当成后端可依赖的前提**，后端侧的义务止于「广播字段足够前端判因」；反过来也别因为
//! 「前端已接线」就把本域文档写成「后端不需要关心 dashboard 刷新」。
//!
//! ## ⚠️ matchit 注册顺序：**无冲突**
//!
//! 本域全部路由是**静态段**：只有 `/release-terminal` 一条，**无 `Path<i64>`
//! catch-all**（本域没有任何 path 参数端点）⇒ 不受「同段数时静态段必须先于
//! catch-all，否则静态路径被吞成 400」那条硬约束。口径同 `/mail-digest` 那条登记。

use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use crate::auth::rbac::CurrentUser;
use crate::infra::ws_hub::WsEvent;
use crate::modules::serial::dto::{ReleaseTerminalOut, ReleaseTerminalRequest};
use crate::modules::serial::service;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/serials/release-terminal` → 200 OK
///
/// **2026-10-19 新增。** MANAGER **单角色**一键释放「已完成 / 已取消但仍占着序列号」
/// 的工单，**装配件（`t_assembly`）+ 零件 / 子件（`t_part`）两张表一起清**。
/// 用途是补上历史数据里那些从未走过状态机派生链的序列号欠账 —— 序列号被
/// `uk_t_assembly_serial_no` / `uk_t_part_serial_no` 守着，**泄漏即永久占位**，
/// 新工单取号会被直接挡住。
///
/// 行为：
/// - 事务：`dry_run=false` 时 `pool.begin()` → service → 显式 `tx.commit()`；
///   `dry_run=true` 走 `pool.acquire()` **不开事务**、零写入。
/// - WS 广播：commit 之后**一条**汇总 `TERMINAL_SERIALS_RELEASED`
///   （payload 只有 `assembly_count` / `part_count` 两个计数；dry_run 不广播）。
/// - 响应：`ReleaseTerminalOut`（`dry_run` + 两个分表计数 + `released_total`
///   + `truncated` + `affected` 明细，明细封顶 200 行而计数是真实总数）。
///
/// **角色闸**：`require_role(Manager)`（判在 service 第一行，早于全表扫）。
/// **dry_run 语义**：`true` = 只读预览、供前端确认框展示「一共会动多少行」；
/// `false` = 真跑。两者出参同结构。
pub async fn release_terminal_serials(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ReleaseTerminalRequest>,
) -> Result<Json<R<ReleaseTerminalOut>>, AppError> {
    // 预览分支：**不开事务**（纯只读，见本文件模块 doc 的事务边界表）。
    if req.dry_run {
        let mut conn = state.pool.acquire().await?;
        let out = service::release_terminal(&mut conn, &state.snowflake, req, &current).await?;
        return Ok(Json(R::ok(out)));
    }

    // 实跑分支：整批清列 + 子件归档事件同事务，全成功或全回滚。
    let mut tx = state.pool.begin().await?;
    let out = service::release_terminal(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;

    // 广播在 commit **之后**（commit 失败却已广播 = 大屏看到一次从未发生的清理）。
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "TERMINAL_SERIALS_RELEASED".into(),
        payload: json!({
            "assembly_count": out.assembly_count,
            "part_count": out.part_count,
        }),
    });
    Ok(Json(R::ok(out)))
}

/// `serial` 域 axum 子路由（不含公共前缀；由 [`crate::modules::serial::router`]
/// 桥接到 `/serials`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/release-terminal", post(release_terminal_serials))
}
