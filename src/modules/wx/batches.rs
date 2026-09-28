//! 微信小程序 BFF / batches 域 handler（2026-09-28 新增）
//!
//! 2 个端点：
//! - `GET /api/v2/wx/batches/counts?period=YYYY-MM` —— 当月 in_progress / done
//!   计数
//! - `GET /api/v2/wx/batches?tab=in_progress|done&period=&page=&size=` ——
//!   批次卡片分页
//!
//! ## 鉴权：Bearer JWT + Redis session（v2_router 中间件统一处理）
//!
//! ## 事务分层
//! 全部 read-only 端点：`pool.acquire()` 不开事务。

use std::sync::Arc;

use axum::Json as AxumJson;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use serde::Deserialize;

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::repo::{BatchCountsAgg, BatchList, row_to_wx_batch};
use super::resolve_period;
use super::vo::{BatchCounts, WxBatchSummary, WxPage};

/// `/api/v2/wx/batches/*` 入口 router 工厂。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/counts", get(counts))
        .route("/", get(list))
}

/// mini-program 默认 page size（卡片视图，避免首屏 >10 张图过载）。
const DEFAULT_PAGE_SIZE: i64 = 10;
/// max page size（防 list 端点被恶意拉全表）。
const MAX_PAGE_SIZE: i64 = 50;

/// Query 参数共享结构（counts + list 共用）。
#[derive(Debug, Deserialize)]
pub struct BatchesPeriodQuery {
    #[serde(default)]
    pub period: Option<String>,
}

/// `GET /api/v2/wx/batches/counts?period=YYYY-MM`
///
/// period 缺省 → 当前月（`chrono::Local::now() %Y-%m`）。
/// 非法格式 → `40001 VALIDATION_ERROR`。
pub async fn counts(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Query(q): Query<BatchesPeriodQuery>,
) -> Result<AxumJson<R<BatchCounts>>, AppError> {
    let period = resolve_period(q.period.as_deref())?;
    let mut conn = state.pool.acquire().await?;
    let resp = BatchCountsAgg::by_period(&mut conn, &period).await?;
    Ok(AxumJson(R::ok(resp)))
}

/// `GET /api/v2/wx/batches?tab=in_progress|done&period=&page=&size=`
///
/// Query 参数：
/// - `tab`：必填，`in_progress` 或 `done`；非法 → `40001`
/// - `period`：缺省 → 当前月
/// - `page`：默认 1
/// - `size`：默认 10，最大 50
#[derive(Debug, Deserialize)]
pub struct BatchesListQuery {
    pub tab: String,
    #[serde(default)]
    pub period: Option<String>,
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub size: Option<i64>,
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Query(q): Query<BatchesListQuery>,
) -> Result<AxumJson<R<WxPage<WxBatchSummary>>>, AppError> {
    // 1. tab 白名单校验（白名单之外 → 40001）
    let tab = match q.tab.as_str() {
        "in_progress" | "done" => q.tab.as_str(),
        _ => {
            return Err(AppError::validation(format!(
                "tab {q:?} 非法（in_progress / done）"
            )));
        }
    };
    // 2. period 校验（YYYY-MM）
    let period = resolve_period(q.period.as_deref())?;
    // 3. 分页参数
    let page = q.page.unwrap_or(1).max(1);
    let size = q.size.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let offset = (page - 1) * size;

    let mut conn = state.pool.acquire().await?;
    let total = BatchList::count(&mut *conn, tab, &period).await?;
    let rows = BatchList::list(&mut *conn, tab, &period, size, offset).await?;
    let items: Vec<WxBatchSummary> = rows.into_iter().map(row_to_wx_batch).collect();
    Ok(AxumJson(R::ok(WxPage::new(items, total, page, size))))
}
