//! part 域打印 handler（BFF 转发）
//!
//! 2026-10-03 新增：2 个端点为**纯转发**——
//! `GET /parts/{part_id}/print-drawing`（单件图纸 + 条码背面 PDF）与
//! `POST /parts/print-drawing-batch`（多件合并 PDF）。PDF 光栅化 / pikepdf 合并 /
//! ReportLab 条码全在 python 侧执行，rust 只提供「必须带 JWT 才能打印」这道闸门
//! 与一条撑得住分钟级耗时的通道（`middleware::timeout::is_print_path` 长档）。
//!
//! ## handler 语义
//! 1. `authenticate_middleware` 已强制 JWT 校验（未带 token → 40100）；
//! 2. `require_permission("part.drawing.print")` 闸门（2026-10-14 起）：
//!    权限点由 `seeds/rbac.sql` 授予 MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER，
//!    SHELF_ACCOUNT（货架终端，只该扫码）不在其中；ADMIN 由元角色通配放行。
//!    不通过 → 40300。迁移前的角色白名单见 [`require_print_role`] 的 doc；
//! 3. clone `headers` 后注入 `X-Forwarded-User-Id: <CurrentUser.id>`；
//! 4. 调 `state.py_backend.forward_part_print_pdf{,_batch}(...)`，拿
//!    `(status, headers, body)` 三元组原样拼 `Response`。
//!
//! ## 与送货单打印的 RBAC 差异
//! 图纸打印额外放行 `CNC_PROGRAMMER`：CNC 编程岗要看图纸才能编程序，
//! 送货单是单据打印、编程岗用不上（`delivery_note.print` 的矩阵不含 CNC）。
//!
//! ## 三条关键取舍
//! - **不开事务**：纯转发，不读写 DB。
//! - **body / query 原样透传**：批量 body 用 `Json<Value>` 透传（前端雪花 ID 是
//!   string，> 2^53，JSON number 会丢精度）；单件的 `?vector=<bool>` 用
//!   [`RawQuery`] 取**原始 query 串**直接交给 py_backend，rust 侧不解析成 bool
//!   （python 端是 `Query(bool)`，语义由 python 负责；rust 若解析就等于把
//!   python 的默认值 / 兼容性规则复制一份到两处）。
//! - **鉴权头不透传给 python**：`filter_request_headers` 剥掉 `Authorization` /
//!   `Cookie`；身份只通过 `X-Forwarded-User-Id` 单头传递。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::state::AppState;

/// 零件图纸打印闸门：权限点 `part.drawing.print`（2026-10-14 由角色白名单迁移）。
///
/// 权限点授权矩阵（`seeds/rbac.sql`，逐格等于迁移前的 `require_any_role` 集合）：
/// `MANAGER` / `CLERK` / `INSPECTOR` / `CNC_PROGRAMMER`；`SHELF_ACCOUNT` 不授；
/// ADMIN 元角色在 `require_permission` 内短路放行。两条打印 handler 共用本函数，
/// 调用点不变。
fn require_print_role(current: &CurrentUser) -> Result<(), AppError> {
    current.require_permission("part.drawing.print")
}

/// clone `headers` 并注入 `X-Forwarded-User-Id`。
///
/// 形参 `headers` 由 axum extractor 提供，不可直接 mutate（会污染共用同一
/// `HeaderMap` 的其它 extractor / middleware），故先 clone 一份副本再插入。
/// `filter_request_headers` 的 SKIP 列表不含此 header，不会被二次过滤。
fn forwarded_headers(headers: &HeaderMap, current: &CurrentUser) -> HeaderMap {
    let mut fwd = headers.clone();
    if let Ok(value) = current.id.to_string().parse() {
        fwd.insert("x-forwarded-user-id", value);
    }
    fwd
}

/// `GET /api/v2/parts/{part_id}/print-drawing` —— 鉴权 + 转发
///
/// 转发到 python `GET /api/v1/parts/{part_id}/print`（**python 端路径不同名**，
/// 映射在 `infra::py_backend` 的 impl 里）。`?vector=<bool>` 原始 query 串透传。
///
/// python 端 `Content-Disposition` 是 `inline; filename="part-{part_id}.pdf"`；
/// 前端单件打印走 iframe、直接吃浏览器内联渲染，不消费这个文件名。
pub async fn print_drawing(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    require_print_role(&current)?;
    let fwd_headers = forwarded_headers(&headers, &current);
    let resp = state
        .py_backend
        .forward_part_print_pdf(&part_id.to_string(), query, fwd_headers)
        .await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}

/// `POST /api/v2/parts/print-drawing-batch` —— 鉴权 + 转发
///
/// 转发到 python `POST /api/v1/parts/print-batch`（**python 端路径不同名**，
/// 映射在 `infra::py_backend` 的 impl 里）。body 原样透传，python 端按
/// `part_ids` 顺序合并，可选 `assembly_ids` 追加总装图页。
pub async fn print_drawing_batch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    require_print_role(&current)?;
    let fwd_headers = forwarded_headers(&headers, &current);
    let resp = state
        .py_backend
        .forward_part_print_pdf_batch(body, fwd_headers)
        .await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}
