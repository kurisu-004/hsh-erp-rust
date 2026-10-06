//! prod::inspection 子模块 handler 层 —— HTTP 路由
//!
//! 2026-10-05 新增：单只读端点。
//!
//! 2026-10-07 新增：待品检队列读端点（随路由由 `GET /api/v2/prod/batches/inspection`
//! 迁到本域 `GET /api/v2/prod/inspection/queue`）。
//!
//! ## 端点
//! - `GET /api/v2/prod/inspection/scan/{serial_no}` —— Manager + Inspector
//! - `GET /api/v2/prod/inspection/queue` —— Manager + Inspector
//!
//! ## 事务边界
//! 读端点：`pool.acquire()` 不开事务，**不发** WS 广播（纯查询，无业务流转）。
//!
//! ## 角色守卫
//! 在 service 第一行下沉（沿 `prod::batch` 范本），handler 不重复校验。
//!
//! ## 路径参数
//! `serial_no` 是 `Path<String>`（不是 `ni64!` 那类数值提取器）：序列号是
//! `varchar(15)` 的**字符串**，且可能含 `-`（子件 `{asm}-{i:02d}`），不能用数值
//! 提取器。空串 / 尾随空白由 service 统一 trim + 兜底（空串按未命中 → 20101）。
//!
//! ⚠️ **前端必须对 `serial_no` 做 `encodeURIComponent`**（已进 API 契约）：序列号
//! 是用户/业务侧自由文本，含 URL 保留字符时行为分两种 ——
//!   - 含 `/`：**未**编码时 axum 路由在 `/scan/{serial_no}` 这一段就把它当路径分隔符
//!     拆开 → 匹配不到路由 → **HTTP 404 且响应体为空**（`v2_router()` 未挂
//!     `.fallback(...)`，拿不到任何信封，前端 `res.json()` 会直接抛解析异常，不是
//!     「拿到一个 code ≠ 20101 的信封」）；**正确**编码成 `%2F` 时路由按单段匹配、
//!     `Path` 解码回含 `/` 的序列号，正常按 20101 命中 / 未命中收口
//!     （见集成测试场景 13）；
//!   - 含 `?` / `#`：未编码时会在客户端或代理层被当成 query / fragment 起始符 →
//!     传进来的序列号被**截断**，同样表现为「扫到了但不对的码」。
//!
//! 本端点**不做**任何解码侧的兜底（不剥离非法字符、不做 `%` 转义还原），因为无法
//! 区分「用户真敲了一个 `%`」与「前端忘了编码」。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::InspectionQueueQuery;
use super::service::{InspectionQueueService, InspectionScanService};
use super::vo::{InspectionQueueListOut, ScanTreeOut};

/// `GET /api/v2/prod/inspection/scan/{serial_no}`
///
/// 扫码查询：返回「装配件（可空）→ 全部子件 → 全部批次」三层树。
///
/// 角色：Manager + Inspector（service 内守卫）。
/// 读端点：`pool.acquire()` 不开事务，不发 WS 广播。
/// ⚠️ `serial_no` 含 URL 保留字符时前端必须 `encodeURIComponent`（含 `/` 未编码会在路由
/// 层 404 且**响应体为空**、拿不到信封），详见模块 doc 的「路径参数」节。
pub async fn scan(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(serial_no): Path<String>,
) -> Result<Json<R<ScanTreeOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = InspectionScanService::scan(&mut conn, &current, &serial_no).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/prod/inspection/queue`
///
/// 待品检队列列表（Manager + Inspector）。只读端点：`pool.acquire()` 不开事务。
///
/// 2026-10-07 自 `GET /api/v2/prod/batches/inspection` 迁入本域：出参切到
/// `InspectionQueueListOut`（13 字段），查询参数是 `InspectionQueueQuery`
/// （表头 7 列各一个筛选 + 服务端排序），与 `/repair` / `/repairing` 的宽 VO 彻底
/// 分家。旧路径**无 alias、已下线**（见模块 doc 的「破坏性路由变更」一节）。
pub async fn queue(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<InspectionQueueQuery>,
) -> Result<Json<R<InspectionQueueListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = InspectionQueueService::list_queue(&mut conn, &query, &current).await?;
    Ok(Json(R::ok(out)))
}
