//! outsource 域 HTTP handler（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/api/v1/outsource_*.py。
//!
//! ## 事务边界（2026-09-22 refactor 对齐 iam 范本）
//! handler 负责 `pool.begin()` / `tx.commit()`——与 iam / 11 个其它 handler 文件现状对齐：
//! - ① **纯写端点**（create_company / update_company / soft_delete_company /
//!   set_company_processes / create_quote / update_quote / submit_quote / approve_quote /
//!   reject_quote / soft_delete_quote / reconcile_update_shipment）：
//!   `pool.begin()` → service call → `tx.commit()`，错误路径 tx drop 隐式回滚。
//! - ② **写 + post-commit 副作用**：outsource 域当前无 Redis / WS 副作用需求，
//!   故全部写端点走形态 ①。
//! - ③ **读端点**（list_companies / get_company / list_companies_by_process /
//!   list_quotes / get_quote）：`pool.acquire()` 不开事务，
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
//!
//! ## 路由表（22 端点）
//!
//! - `GET    /outsource-companies`              — 列表（READ）
//! - `POST   /outsource-companies`              — 新建（WRITE）
//! - `GET    /outsource-companies/{id}`         — 详情
//! - `GET    /outsource-companies/{id}/sent-parts` — 对账页 sent-parts 一览（2026-10-03 新增）
//! - `POST   /outsource-companies/{id}/update`  — 更新（OCC）
//! - `POST   /outsource-companies/{id}/soft-delete` — 软删
//! - `GET    /outsource-companies/by-process/{process_id}` — 按工序反查
//! - `POST   /outsource-companies/{id}/processes` — 整体替换工序映射
//!
//! - `GET    /outsource-quotes`                 — 列表
//! - `POST   /outsource-quotes`                 — 新建 DRAFT
//! - `GET    /outsource-quotes/quotable-parts`  — 报价 picker（2026-10-03 新增）
//! - `GET    /outsource-quotes/{id}`            — 详情
//! - `POST   /outsource-quotes/{id}/update`     — 更新 DRAFT
//! - `POST   /outsource-quotes/{id}/submit`     — DRAFT → SUBMITTED
//! - `POST   /outsource-quotes/{id}/approve`    — SUBMITTED → APPROVED (MANAGER-only)
//! - `POST   /outsource-quotes/{id}/reject`     — SUBMITTED → REJECTED (MANAGER-only)
//! - `POST   /outsource-quotes/{id}/soft-delete` — 软删 DRAFT/REJECTED
//!
//! - `GET    /outsource-shipments/in-flight`    — 在途批次一览（2026-10-03 新增）
//! - `POST   /outsource-shipments/{id}/reconcile-update` — 对账页更新
//!
//! 顶层（独立前缀，见 `modules::mod.rs::v2_router`）：
//! - `GET    /outsource-sendable`               — 可发送外协一览（2026-10-03 新增；
//!   **2026-10-09 起随看板接管而待删**，见 `modules/mod.rs` 的 nest 注释）
//! - `GET    /outsource-queue/snapshot`         — 外协工序序列板（2026-10-09 新增）
//! - `GET    /outsource-queue/processes/{id}`   — 单工序看板（候选 + 公司列含在途批次）
//!
//! ### 2026-10-09 下线的三条读端点（硬切无 alias）
//! - `GET /outsource-pool/counts` → `/outsource-queue/snapshot`
//! - `GET /outsource-pool/{process_id}` → `/outsource-queue/processes/{process_id}`
//! - `GET /outsource-pool/state` → 被 `companies[].held_batches` 内联取代

mod board;

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::outsource::dto::{
    OutsourceCompanyCreateRequest, OutsourceCompanyListQuery, OutsourceCompanyUpdateRequest,
    OutsourceInFlightListQuery, OutsourceQuotablePartListQuery, OutsourceQuoteApproveRequest,
    OutsourceQuoteCreateRequest, OutsourceQuoteListQuery, OutsourceQuoteRejectRequest,
    OutsourceQuoteUpdateRequest, OutsourceSendableListQuery, OutsourceSentPartListQuery,
    OutsourceShipmentReconcileUpdateRequest, SetOutsourceCompanyProcessRequest,
};
use crate::modules::outsource::vo::{
    OutsourceCompanyListOut, OutsourceCompanyOut, OutsourceCompanyWithProcessesOut,
    OutsourceInFlightListOut, OutsourceQuoteListOut, OutsourceQuoteOut, OutsourceSendableListOut,
    OutsourceSentPartListOut, OutsourceShipmentOut, QuotablePartListOut,
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
pub async fn create_company(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceCompanyCreateRequest>,
) -> Result<(StatusCode, Json<R<OutsourceCompanyWithProcessesOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .create_company(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
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
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .outsource_service
        .soft_delete_company(&mut *tx, id, &current)
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
) -> Result<Json<R<Vec<OutsourceCompanyOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_companies_for_process(&mut *conn, process_id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-companies/{id}/processes —— 纯写端点
pub async fn set_company_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetOutsourceCompanyProcessRequest>,
) -> Result<Json<R<OutsourceCompanyWithProcessesOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .set_company_processes(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /outsource-companies/{id}/sent-parts`（2026-10-03 新增）—— 读端点
///
/// 2 段路径（`/{id}/sent-parts`），与 1 段的 `/{id}` 无 matchit 冲突。
/// 此前本端点**根本没注册**，前端「外协对账」页恒 404（写侧 reconcile-update
/// 一直存在，只是读不到数据）。
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

/// GET /outsource-quotes/{id} —— 读端点，acquire 不开事务
pub async fn get_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .get_quote(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/update —— 纯写端点
pub async fn update_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<OutsourceQuoteUpdateRequest>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .update_quote(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /outsource-quotes/{id}/submit —— 纯写端点
pub async fn submit_quote(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<OutsourceQuoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .outsource_service
        .submit_quote(&mut *tx, id, &current)
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
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .outsource_service
        .soft_delete_quote(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

/// `GET /outsource-quotes/quotable-parts`（2026-10-03 新增）—— 读端点
///
/// ⚠️ **必须注册在 `quote_router()` 的 `/{id}` 之前**。此前本端点未注册，
/// 请求被 `/{id}`（`Path<i64>`）吞掉 → `PathRejection` → 恒 400（前端报价一览页
/// 每次进都报错、「新建报价」picker 恒空）。
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
//  Sendable（2026-10-03 新增，独立顶层前缀 `/outsource-sendable`）
// ===========================================================================

/// `GET /outsource-sendable` —— 读端点
///
/// 一行 = 一个（活跃批次 × OUTSOURCE 工序）组合，`send_mode` 判 APPROVAL / DIRECT
/// 由 SQL 一次判定（见 `OutsourceSendableRepo`）。角色守卫与旧
/// `/parts/outsource-sendable` 一致。
pub async fn list_sendable(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<OutsourceSendableListQuery>,
) -> Result<Json<R<OutsourceSendableListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .outsource_service
        .list_sendable(&mut *conn, &query, &current)
        .await?;
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
        .route("/{id}/processes", post(set_company_processes))
        // 2026-10-03 新增：对账页 sent-parts（2 段路径，与 1 段 `/{id}` 无冲突）
        .route("/{id}/sent-parts", get(list_company_sent_parts))
        .route("/{id}", get(get_company))
}

/// Quote 路由（挂载点 `/outsource-quotes`）
pub fn quote_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_quotes).post(create_quote))
        // 2026-10-03 新增：**静态段必须在 `/{id}` catch-all 之前注册**，否则
        // `quotable-parts` 会被 `Path<i64>` 吞掉（400 PathRejection）。
        .route("/quotable-parts", get(list_quotable_parts))
        .route("/{id}/update", post(update_quote))
        .route("/{id}/submit", post(submit_quote))
        .route("/{id}/approve", post(approve_quote))
        .route("/{id}/reject", post(reject_quote))
        .route("/{id}/soft-delete", post(soft_delete_quote))
        .route("/{id}", get(get_quote))
}

/// Shipment 路由（挂载点 `/outsource-shipments`）
pub fn shipment_router() -> Router<Arc<AppState>> {
    Router::new()
        // 2026-10-03 新增：1 段静态段（与 2 段的 `/{id}/reconcile-update` 无冲突）
        .route("/in-flight", get(list_in_flight))
        .route("/{id}/reconcile-update", post(reconcile_update_shipment))
}

/// Sendable 路由（挂载点 `/outsource-sendable`，**独立顶层前缀**）
///
/// 2026-10-03 新增。`/outsource-sendable` 不是 quote / shipment / company 任何
/// 单一域的子资源（「可发送外协的批次」横跨全部三者），故不 nest 进既有 3 个
/// router，而是顶层独立前缀 —— 命名沿用旧的 `/parts/outsource-sendable`，便于
/// 前端对照迁移。
pub fn sendable_router() -> Router<Arc<AppState>> {
    Router::new().route("/", get(list_sendable))
}

/// 外协看板路由（挂载点 `/outsource-queue`，见 `modules::v2_router`）。
///
/// ⚠️ **本 router 的两条 route 段数不同，注册顺序无硬约束**：`/snapshot` 是 1 段
/// 静态段，`/processes/{process_id}` 是 2 段，matchit 按段位匹配，两者不争同一段位。
/// 这与被取代的 `pool_router`（`/counts` `/state` `/{process_id}` **全是 1 段**）正
/// 相反 —— 那里静态段必须先注册，否则参数段 `/{process_id}` 会兜住任何未命中的单段
/// 静态路径，再由 `Path<i64>` 反序列化拒绝 → **400** 而非 404。同一坑在
/// `quote_router()` 的 `quotable-parts` 与 part 域的 `/{part_id}` 上都踩过。
pub fn queue_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/snapshot", get(board::snapshot))
        .route("/processes/{process_id}", get(board::process_detail))
}
