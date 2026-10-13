//! part 域 lifecycle / 状态机扩展 handler
//!
//! 本文件只服务 part 维度的 2 条写端点：
//! - `POST /api/v2/parts/{part_id}/cancel`
//! - `POST /api/v2/parts/{part_id}/force-complete`
//!
//! 以**单个批次**为操作对象的端点不在 part 域：deliver / complete /
//! place-on-shelf / recall-to-pending / split-batch / cancel-batch /
//! release-from-programming / 外协 3 条 / 返修 2 条 / start-repair / pick-up，
//! 见 `crate::modules::prod::batch::handler::lifecycle`（URL 挂
//! `/api/v2/prod/batches/*`）—— 批次级动作按批次做权限与状态机判定更贴合语义，
//! 留在 part 域会让「谁有资格翻状态」这件事跨两个域分裂。
//!
//! WS 广播：commit 之后广播（对齐 Python 延迟广播模式）；事件名与 Python 一致。
//!
//! ## 权限模式
//! - cancel：`require_any_role([Manager, Clerk])` —— 撤销属仓库动作，放开 Clerk
//! - force-complete：`require_role(Manager)` —— 逃生通道（绕状态机强推），明确
//!   不下放 Clerk
//!
//! ## 移除记录（2026-10-13：零消费方端点整条下线）
//!
//! - `GET /api/v2/parts/by-work-type/{work_type_id}`（连同它的
//!   `service/phase1/work_type.rs` 整个文件与 `dto_crud::ByWorkTypeQuery`）
//!   —— **零消费方**：前端 `src/types/parts.ts` 里只有一句类型注释、**没有任何
//!   api 函数**，而工种维度的三个页面数据源全是报工台那两条端点
//!   （`GET /api/v2/prod/scan/pickable` / `held`）。
//!   ⚠️ **旧路径是干净 404、不是 400**（本域「段数是分水岭」那条规则的 2 段一侧）：
//!   `by-work-type/{id}` 是**两段**，而 `Path<i64>` catch-all 是 1 段的
//!   `/{part_id}`，本 router 全部 2 段路由的第二段都是**字面量**（`update` /
//!   `cancel` / `events` / …），没有 `/{part_id}/{param}` 这种形态 ⇒ 删路由后
//!   没有任何注册路径能匹配它。⚠️ 相反，1 段静态路径才会落进 `/{part_id}` 被
//!   `Path<i64>` 拒成 **400 纯文本** —— 同批删除的
//!   `POST /api/v2/parts/batch-with-pdfs` 正是那一种，两条形态别记混。
//! - 2026-10-10 早已迁往 `prod::scan` 的 `pickable-by-work-type` / `by-worker`
//!   （新路径 `GET /api/v2/prod/scan/pickable` 与 `held`，**无 alias**）—— 本轮
//!   一并在此登记，便于后人查本域 list 族的去向。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::dto_crud::{CancelRequest, ForceCompleteRequest};
use crate::modules::part::service::PartService;
use crate::modules::part::vo::PartOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 通用 WS 广播 helper：单 kind + 单 payload 字段。
#[inline]
fn ws_broadcast(state: &AppState, kind: &str, payload: serde_json::Value) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: kind.into(),
        payload,
    });
}

/// `POST /api/v2/parts/{part_id}/cancel`
///
/// 5 状态白名单 → CANCELLED；commit 后广播 `PART_CANCELLED`。
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::cancel(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_CANCELLED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/force-complete`（2026-09-30 新增）
///
/// MANAGER **单角色** 强推工单 + 该工单下所有活跃批次为 COMPLETED（绕状态机）。
/// 事件日志 `FORCE_COMPLETED` 区别常规 COMPLETED；commit 后广播
/// `PART_FORCE_COMPLETED`。
pub async fn force_complete(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ForceCompleteRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::force_complete(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_FORCE_COMPLETED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}
