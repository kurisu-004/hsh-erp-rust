//! 微信小程序 BFF / parts 域 handler（2026-09-28 新增）
//!
//! 3 个端点：
//! - `GET /api/v2/wx/parts/counts` —— 工单 4 tab 计数
//! - `GET /api/v2/wx/parts?status=&page=&size=` —— 工单卡片分页
//! - `GET /api/v2/wx/parts/by-serial/{serial_no}` —— 扫码定位单件
//!
//! ## 鉴权：Bearer JWT + Redis session（v2_router 中间件统一处理）
//!
//! ## 事务分层
//! 全部 read-only 端点：`pool.acquire()` 不开事务。

use std::sync::Arc;

use axum::Json as AxumJson;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::Deserialize;

use crate::auth::rbac::CurrentUser;
use crate::modules::part::statemachine::PartStatus;
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::shared::types::deserialize_i64_opt;
use crate::state::AppState;

use super::repo::{PartCounts, PartList, map_counts_by_status, row_to_wx_part};
use super::vo::{CountsByStatus, WxPage, WxPartSummary};

/// `/api/v2/wx/parts/*` 入口 router 工厂。
///
/// 路由顺序：
/// - 静态段 `/counts`（先注册，避免被 `by-serial/{serial_no}` catch-all 吃掉）
/// - 动态段 `/by-serial/{serial_no}`（catch-all 形）
/// - 兜底 `/`（list 分页）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/counts", get(counts))
        .route("/by-serial/{serial_no}", get(by_serial))
        .route("/", get(list))
}

/// `GET /api/v2/wx/parts/counts`
///
/// 与 `/wx/dashboard/home.part_counts` 同 SQL（共用 `PartCounts::by_status`）。
pub async fn counts(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
) -> Result<AxumJson<R<CountsByStatus>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let raw = PartCounts::by_status(&mut *conn).await?;
    Ok(AxumJson(R::ok(map_counts_by_status(raw))))
}

/// `GET /api/v2/wx/parts?status=&customer_id=&page=&size=`
///
/// Query 参数：
/// - `status`：可选；`all` / 不传 → 不过滤；其它值必须命中
///   `PartStatus::from_str` 白名单（10 个值）否则 40001
/// - `customer_id`：可选（mini-program「客户视角筛选」备用，本 PR 不实现跨客户
///   权限隔离，按需后续加）
/// - `page`：默认 1
/// - `size`：默认 10（mini-program 卡片不大于 10 防首屏过载）
#[derive(Debug, Deserialize)]
pub struct PartsListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub size: Option<i64>,
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Query(q): Query<PartsListQuery>,
) -> Result<AxumJson<R<WxPage<WxPartSummary>>>, AppError> {
    // status 校验（None / "all" → 不传；其它走白名单）
    let status_filter: Option<&str> = match q.status.as_deref() {
        None => None,
        Some(s) if s.eq_ignore_ascii_case("all") => None,
        Some(s) => {
            if PartStatus::from_str(s).is_none() {
                return Err(AppError::validation(format!(
                    "status {s:?} 不在白名单（PENDING/IN_PROCESS/...）"
                )));
            }
            Some(s)
        }
    };

    let page = q.page.unwrap_or(1).max(1);
    let size = q.size.unwrap_or(10).clamp(1, 50);
    let offset = (page - 1) * size;

    let mut conn = state.pool.acquire().await?;
    let total = PartList::count(&mut *conn, status_filter, q.customer_id).await?;
    let rows = PartList::list(&mut *conn, status_filter, q.customer_id, size, offset).await?;
    let items: Vec<WxPartSummary> = rows.into_iter().map(row_to_wx_part).collect();
    Ok(AxumJson(R::ok(WxPage::new(items, total, page, size))))
}

/// `GET /api/v2/wx/parts/by-serial/{serial_no}`
///
/// 扫码定位单件：
/// - 找不到 / 已软删 → `40400 NOT_FOUND`（保留 `BIZ_PART_NOT_FOUND` 20101
///   也可，但 mini-program 前端只关心「找不到」，用 NOT_FOUND 更直观）
/// - 命中 → `WxPartSummary`（与 list 响应同 shape，方便前端复用渲染组件）
pub async fn by_serial(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Path(serial_no): Path<String>,
) -> Result<AxumJson<R<WxPartSummary>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let row = PartList::by_serial(&mut *conn, &serial_no)
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::NOT_FOUND,
                format!("serial_no {serial_no} 不存在或已删除"),
            )
        })?;
    Ok(AxumJson(R::ok(row_to_wx_part(row))))
}
