//! delivery_note 域打印 handler（BFF 转发）
//!
//! 2026-10-03：2 个端点改为**纯转发**——`POST /delivery-notes/{id}/print` 与
//! `POST /delivery-notes/{id}/print-labels` 只做「鉴权 + 闸门 + 转发」，渲染动作
//! （模板填表 / 标签生成）全在 python 侧执行。
//!
//! ## handler 语义
//! 1. `authenticate_middleware` 已强制 JWT 校验（未带 token → 40100）；
//! 2. `require_any_role` 限制 `MANAGER / CLERK / INSPECTOR`（对齐前端 `canPrint`
//!    闸门 + python 端历史 RBAC）；不通过 → 40300；
//! 3. clone `headers` 后注入 `X-Forwarded-User-Id: <CurrentUser.id>`；
//! 4. 调 `state.py_backend.forward_delivery_note_print{,_labels}(id, body, headers)`，
//!    拿 `(status, headers, body)` 三元组原样拼 `Response`。
//!
//! ## 三条关键取舍
//! - **不开事务**：纯转发，不读写 DB。`state.pool` 在本文件里不出现。
//! - **body 用 `Json<Value>` 原样透传**：不定义强类型 DTO、不解析字段。前端发的
//!   雪花 ID 是 string（> 2^53，JSON number 会丢精度），rust 侧解析只会引入
//!   一层无收益的转换；`custom_order` / `merge_quantities` / `line_item_ids` 的
//!   语义由 python 端 schema 负责（与 STS 转发同构）。
//! - **鉴权头不透传给 python**：`filter_request_headers`（`infra::py_backend`）
//!   剥掉 `Authorization` / `Cookie`，python 端不反向依赖 rust 的 JWT。身份只
//!   通过 `X-Forwarded-User-Id` 单头传递。
//!
//! ## 响应头
//! 由 `filter_response_headers` 清洗：保留 `content-type` / `content-disposition`
//! （前端 `parseFilename` 靠后者取下载文件名）/ `cache-control`，`content-length`
//! 按实际 body 长度重算；hop-by-hop 与 `content-encoding` 剥除。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::delivery_note::dto::DeliveryNotePath;
use crate::shared::error::AppError;
use crate::state::AppState;

/// 打印允许的角色集：`MANAGER` / `CLERK` / `INSPECTOR`。
///
/// 与前端 `canPrint` 闸门、python 端历史 RBAC 三方对齐；`SHELF_ACCOUNT`
/// （货架终端）不放行——它只该扫码，不该开单打印。
fn require_print_role(current: &CurrentUser) -> Result<(), AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])
}

/// 2026-10-03 新增：clone `headers` 并注入 `X-Forwarded-User-Id`。
///
/// 形参 `headers` 由 axum extractor 提供，**不可**直接 mutate（会污染共用同一
/// `HeaderMap` 的其它 extractor / middleware），故先 clone 一份副本再插入。
/// `filter_request_headers` 的 SKIP 列表不含此 header，不会被二次过滤。
fn forwarded_headers(headers: &HeaderMap, current: &CurrentUser) -> HeaderMap {
    let mut fwd = headers.clone();
    if let Ok(value) = current.id.to_string().parse() {
        fwd.insert("x-forwarded-user-id", value);
    }
    fwd
}

/// `POST /api/v2/delivery-notes/{id}/print` —— 鉴权 + 转发
///
/// 转发到 python `POST /api/v1/delivery-notes/{id}/print`（路径同名），
/// 拿回 xlsx 字节流原样返回。
pub async fn print_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    require_print_role(&current)?;
    let fwd_headers = forwarded_headers(&headers, &current);
    let note_id = path.id.to_string();
    let resp = state
        .py_backend
        .forward_delivery_note_print(&note_id, body, fwd_headers)
        .await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}

/// `POST /api/v2/delivery-notes/{id}/print-labels` —— 鉴权 + 转发
///
/// 转发到 python `POST /api/v1/delivery-notes/{id}/print-labels`（路径同名）。
pub async fn print_labels(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    require_print_role(&current)?;
    let fwd_headers = forwarded_headers(&headers, &current);
    let note_id = path.id.to_string();
    let resp = state
        .py_backend
        .forward_delivery_note_labels(&note_id, body, fwd_headers)
        .await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}
