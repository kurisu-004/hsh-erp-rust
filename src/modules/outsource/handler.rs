//! outsource 域 HTTP handler（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/api/v1/outsource_*.py。
//!
//! ## 事务边界（2026-09-22 refactor 对齐 iam 范本）
//! handler 负责 `pool.begin()` / `tx.commit()`——与 iam / 11 个其它 handler 文件现状对齐：
//! - ① **纯写端点**（create_company / update_company / soft_delete_company /
//!   create_quote / submit_quote / approve_quote / reject_quote / soft_delete_quote /
//!   reconcile_update_shipment）：`pool.begin()` → service call → `tx.commit()`，
//!   错误路径 tx drop 隐式回滚。
//! - ② **写 + post-commit 副作用**：外协看板移动端点（`POST /outsource-queue/move`，
//!   见 `handler/move.rs`）—— `tx.commit()` 之后广播 `OUTSOURCE_MOVE_DONE`。
//! - ③ **读端点**（list_companies / get_company / list_companies_by_process /
//!   list_quotes / 看板两读）：`pool.acquire()` 不开事务，
//!   service 借 `&mut *conn` 执行查询，用完即 drop。
//!
//! service 形参：`repo: R: OutsourceRepoTrait`（by-value）。生产路径
//! `R = &mut PgConnection`，trait `OutsourceRepoTrait` 已直接对 `&mut PgConnection`
//! 实现（见 `repo/mod.rs`）。`OutsourceService` 字段仅 `Arc<SnowflakeIdGenerator>`，
//! 由 `state.outsource_service` 注入。
//!
//! ## 统一响应信封
//! handler 返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//! 装进同一个 `R` 信封。
//!
//! ## 权限
//! 权限守卫在 service 层（`current.require_any_role`），handler 不重复校验。
//!
//! ## 子模块
//! - `board.rs` —— 外协看板只读聚合 2 端点（`snapshot` / `processes/{process_id}`）
//! - `move.rs` —— 外协看板三合一移动写端点（`POST /move`，2026-10-09 新增）
//!
//! ## 路由表（22 端点 → 18 端点）
//!
//! - `GET    /outsource-companies`              — 列表（READ）
//! - `POST   /outsource-companies`              — 新建（WRITE）
//! - `GET    /outsource-companies/{id}`         — 详情（含工序映射）
//! - `GET    /outsource-companies/{id}/sent-parts` — 对账页 sent-parts 一览
//! - `POST   /outsource-companies/{id}/update`  — 更新（OCC + 可选整体替换工序映射）
//! - `POST   /outsource-companies/{id}/soft-delete` — 软删（OCC）
//! - `GET    /outsource-companies/by-process/{process_id}` — 按工序反查（窄 VO）
//!
//! - `GET    /outsource-quotes`                 — 列表
//! - `POST   /outsource-quotes`                 — 新建 DRAFT
//! - `GET    /outsource-quotes/quotable-parts`  — 报价 picker
//! - `POST   /outsource-quotes/{id}/submit`     — DRAFT → SUBMITTED（OCC）
//! - `POST   /outsource-quotes/{id}/approve`    — SUBMITTED → APPROVED (MANAGER-only)
//! - `POST   /outsource-quotes/{id}/reject`     — SUBMITTED → REJECTED (MANAGER-only)
//! - `POST   /outsource-quotes/{id}/soft-delete` — 软删 DRAFT/REJECTED（OCC）
//!
//! - `GET    /outsource-shipments/in-flight`    — 在途批次一览
//! - `POST   /outsource-shipments/{id}/reconcile-update` — 对账页更新
//!
//! 顶层（独立前缀，见 `modules::mod.rs::v2_router`）：
//! - `GET    /outsource-queue/snapshot`         — 外协工序序列板
//! - `GET    /outsource-queue/processes/{id}`   — 单工序看板（候选 + 公司列含在途批次）
//! - `POST   /outsource-queue/move`             — 三合一移动写端点
//!
//! ### 2026-10-09 硬切下线的端点（无 alias）
//! - `GET /outsource-pool/counts` → `/outsource-queue/snapshot`
//! - `GET /outsource-pool/{process_id}` → `/outsource-queue/processes/{process_id}`
//! - `GET /outsource-pool/state` → 被 `companies[].held_batches` 内联取代
//! - `GET /outsource-sendable` → 被 `/outsource-queue/processes/{id}` 的候选列取代
//!   （它只是同一批行的分页子集）
//! - `POST /prod/batches/{id}/send-to-outsource` / `receive-from-outsource` /
//!   `receive-from-outsource-to-inspection` → 三合一为 `POST /outsource-queue/move`
//! - `POST /outsource-companies/{id}/processes` → 吸收进 `POST /{id}/update` 的
//!   `process_ids`
//! - `GET /outsource-quotes/{id}` / `POST /outsource-quotes/{id}/update` → 删除
//!   （前端零消费）

mod board;
// `move` 是 Rust 关键字，不能直接作模块名；文件仍叫 `move.rs`（与
// `service/move.rs` 同名对称），故显式给 `#[path]`。注意本文件不是 `mod.rs`，
// 显式 path 相对的是**本文件所在目录**（`outsource/`）而不是 `handler/`
#[path = "handler/move.rs"]
mod move_batch;

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::outsource::dto::{
    OutsourceCompanyCreateRequest, OutsourceCompanyListQuery, OutsourceCompanySoftDeleteRequest,
    OutsourceCompanyUpdateRequest, OutsourceInFlightListQuery, OutsourceQuotablePartListQuery,
    OutsourceQuoteApproveRequest, OutsourceQuoteCreateRequest, OutsourceQuoteListQuery,
    OutsourceQuoteRejectRequest, OutsourceQuoteSoftDeleteRequest, OutsourceQuoteSubmitRequest,
    OutsourceSentPartListQuery, OutsourceShipmentReconcileUpdateRequest,
};
use crate::modules::outsource::vo::{
    OutsourceCompanyListOut, OutsourceCompanyOptionOut, OutsourceCompanyWithProcessesOut,
    OutsourceInFlightListOut, OutsourceQuoteListOut, OutsourceQuoteOut, OutsourceSentPartListOut,
    OutsourceShipmentOut, QuotablePartListOut,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

// ===========================================================================
//  Company
// ===========================================================================

/// GET /outsource-companies —— 读端点，acquire 不开事务
pub async fn list_companies(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceCompanyListQuery>,
) -> Result<Json<R<OutsourceCompanyListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_companies(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies → 201 —— 纯写端点
///
/// 出参是 `R<()>`（`data: null`）：前端建完公司后一律重拉列表，建号所需的 id 从
/// `GET /outsource-companies?name_like=…` 的首行取。返整份
/// `OutsourceCompanyWithProcessesOut` 的代价是**每次建号多一次工序映射 + 工序元数据
/// 的往返**，而消费方一个字段都不用。
pub async fn create_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceCompanyCreateRequest>,
) -> Result<(StatusCode, Json<R<()>>), AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .outsource_service
        .create_company(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok_empty())))
}

/// GET /outsource-companies/{id} —— 读端点，acquire 不开事务
pub async fn get_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .get_company(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/update —— 纯写端点
pub async fn update_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceCompanyUpdateRequest>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .update_company(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/soft-delete —— 纯写端点
pub async fn soft_delete_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceCompanySoftDeleteRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .outsource_service
        .soft_delete_company(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

/// GET /outsource-companies/by-process/{process_id}
///
/// 静态段必须在 `/{company_id}` catch-all 之前注册。
/// 读端点，acquire 不开事务
pub async fn list_companies_by_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(process_id): Path<i64>,
) -> Result<Json<R<Vec<OutsourceCompanyOptionOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_companies_for_process(&mut *conn, process_id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// `GET /outsource-companies/{id}/sent-parts` —— 读端点
///
/// 2 段路径（`/{id}/sent-parts`），与 1 段的 `/{id}` 无 matchit 冲突。
/// 出参信封带 `outsource_company_id` + `outsource_company_name`，前端用它渲染页头，
/// 不必再单独发一次 `GET /outsource-companies/{id}`。
pub async fn list_company_sent_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Query(query): Query<OutsourceSentPartListQuery>,
) -> Result<Json<R<OutsourceSentPartListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_company_sent_parts(&mut *conn, id, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

// ===========================================================================
//  Quote
// ===========================================================================

/// GET /outsource-quotes —— 读端点，acquire 不开事务
pub async fn list_quotes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceQuoteListQuery>,
) -> Result<Json<R<OutsourceQuoteListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_quotes(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes → 201 —— 纯写端点
pub async fn create_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceQuoteCreateRequest>,
) -> Result<(StatusCode, Json<R<OutsourceQuoteOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .create_quote(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// POST /outsource-quotes/{id}/submit —— 纯写端点
pub async fn submit_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteSubmitRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .submit_quote(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/approve  (MANAGER-only via service) —— 纯写端点
pub async fn approve_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteApproveRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .approve_quote(
            &mut *tx,
            id,
            req.review_note.as_deref(),
            req.version,
            &current,
        )
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/reject  (MANAGER-only via service) —— 纯写端点
pub async fn reject_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteRejectRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .reject_quote(&mut *tx, id, &req.review_note, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/soft-delete —— 纯写端点
pub async fn soft_delete_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteSoftDeleteRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .outsource_service
        .soft_delete_quote(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

/// `GET /outsource-quotes/quotable-parts` —— 读端点
///
/// ⚠️ **必须注册在 `quote_router()` 的 `/{id}` 之前**。当前 `quote_router()` 里已
/// **没有** `/{id}` 路由（2026-10-09 删除），这条约束随之失效 —— 但它是「将来谁
/// 想加回一条 1 段静态路由」时的陷阱登记，故保留在注释里。
pub async fn list_quotable_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceQuotablePartListQuery>,
) -> Result<Json<R<QuotablePartListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_quotable_parts(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

// ===========================================================================
//  Shipment
// ===========================================================================

/// `GET /outsource-shipments/in-flight`（2026-10-03 新增）—— 读端点
///
/// 1 段静态路径，与 2 段的 `/{id}/reconcile-update` 无 matchit 冲突。
/// 取代 part 域旧 `/parts/outsource-in-flight`（返回通用 `PartListItem`，
/// 形状不匹配导致前端在途 tab 空白）。
pub async fn list_in_flight(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceInFlightListQuery>,
) -> Result<Json<R<OutsourceInFlightListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_in_flight(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-shipments/{id}/reconcile-update —— 纯写端点
pub async fn reconcile_update_shipment(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceShipmentReconcileUpdateRequest>,
) -> Result<Json<R<OutsourceShipmentOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .reconcile_update_shipment(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

// ===========================================================================
//  Router（注意静态段必须在 catch-all `/{id}` 之前注册）
// ===========================================================================

/// Company 路由（挂载点 `/outsource-companies`）
pub fn company_router() -> Router<Arc<AppState>> {
    Router::new()
        // 静态段必须在 `/{id}` catch-all 之前
        .route("/by-process/{process_id}", get(list_companies_by_process))
        .route("/", get(list_companies).post(create_company))
        .route("/{id}/update", post(update_company))
        .route("/{id}/soft-delete", post(soft_delete_company))
        // 对账页 sent-parts（2 段路径，与 1 段 `/{id}` 无冲突）
        .route("/{id}/sent-parts", get(list_company_sent_parts))
        .route("/{id}", get(get_company))
}

/// Quote 路由（挂载点 `/outsource-quotes`）
///
/// 2026-10-09 删除 `GET /{id}` 与 `POST /{id}/update`（前端零消费，硬切无 alias），
/// 端点 **9 → 7**。删除后本 router 只剩 1 段静态 `/quotable-parts` 与 2 段
/// `/{id}/*`，段数不同 ⇒ matchit 无同段位争用 ⇒ **注册顺序不再有硬约束**。
pub fn quote_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_quotes).post(create_quote))
        .route("/quotable-parts", get(list_quotable_parts))
        .route("/{id}/submit", post(submit_quote))
        .route("/{id}/approve", post(approve_quote))
        .route("/{id}/reject", post(reject_quote))
        .route("/{id}/soft-delete", post(soft_delete_quote))
}

/// Shipment 路由（挂载点 `/outsource-shipments`）
pub fn shipment_router() -> Router<Arc<AppState>> {
    Router::new()
        // 2026-10-03 新增：1 段静态段（与 2 段的 `/{id}/reconcile-update` 无冲突）
        .route("/in-flight", get(list_in_flight))
        .route("/{id}/reconcile-update", post(reconcile_update_shipment))
}

/// 外协看板路由（挂载点 `/outsource-queue`，见 `modules::v2_router`）。
///
/// ⚠️ **本 router 的三条 route 段数不同，注册顺序无硬约束**：`/snapshot` 与 `/move`
/// 是 1 段静态段，`/processes/{process_id}` 是 2 段，matchit 按段位匹配，两者不争同一
/// 段位。被取代的 `pool_router`（`/counts` `/state` `/{process_id}` **全是 1 段**）
/// 恰好相反 —— 那里静态段必须先注册，否则参数段 `/{process_id}` 会兜住任何未命中的单段
/// 静态路径，再由 `Path<i64>` 反序列化拒绝 → **400** 而非 404。同一坑在
/// `quote_router()` 的 `quotable-parts` 与 part 域的 `/{part_id}` 上都踩过。
pub fn queue_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/snapshot", get(board::snapshot))
        .route("/processes/{process_id}", get(board::process_detail))
        .route("/move", post(move_batch::move_batch))
}
