//! upload_session 域 HTTP handler
//!
//! 2026-09-18 新增。
//!
//! 7 个 POST handler，全部挂在 `/api/v2/upload-sessions`：
//! - `POST /get-or-create`                                —— 拿 / 创建上传会话
//! - `POST /{session_id}/files:allocate`                  —— 批量分配 tmp_key
//! - `POST /{session_id}/files/{client_ref}/complete`     —— head 校验 + 标 done
//! - `POST /{session_id}/files:remove`                    —— 移除条目 + 异步删 tmp
//! - `POST /{session_id}/renew`                           —— 重签 STS
//! - `POST /{session_id}/consume`                         —— 仅移除条目（tmp 删由 confirm/batch 负责）
//! - `POST /{session_id}/discard`                         —— 整条 DEL

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};

use crate::auth::rbac::CurrentUser;
use crate::modules::upload_session::dto::{
    AllocateFilesIn, AllocateFilesOut, CompleteFileIn, CompleteFileOut, ConsumeFilesIn,
    ConsumeFilesOut, DiscardIn, DiscardOut, GetOrCreateIn, GetOrCreateOut, RemoveFilesIn,
    RemoveFilesOut, RenewIn, RenewOut,
};
use crate::modules::upload_session::service::UploadSessionService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /upload-sessions/get-or-create` → 200 OK
pub async fn get_or_create(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<GetOrCreateIn>,
) -> Result<Json<R<GetOrCreateOut>>, AppError> {
    let out = UploadSessionService::get_or_create(
        state.upload_session_repo.clone(),
        state.python_sts.clone(),
        &state.config.upload_session,
        &req,
        &current,
    )
    .await?;
    Ok(Json(R::ok(out)))
}

/// `POST /upload-sessions/{session_id}/files:allocate` → 200 OK
pub async fn allocate_files(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(session_id): Path<String>,
    Json(req): Json<AllocateFilesIn>,
) -> Result<Json<R<AllocateFilesOut>>, AppError> {
    let out = UploadSessionService::allocate_files(
        state.upload_session_repo.clone(),
        state.config.upload_session.ttl_seconds,
        &session_id,
        &req,
        &current,
    )
    .await?;
    Ok(Json(R::ok(out)))
}

/// `POST /upload-sessions/{session_id}/files/{client_ref}/complete` → 200 OK
pub async fn complete_file(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path((session_id, client_ref)): Path<(String, String)>,
    Json(req): Json<CompleteFileIn>,
) -> Result<Json<R<CompleteFileOut>>, AppError> {
    let out = UploadSessionService::complete_file(
        state.upload_session_repo.clone(),
        state.cos.clone(),
        state.config.upload_session.ttl_seconds,
        &session_id,
        &client_ref,
        &req,
        &current,
    )
    .await?;
    Ok(Json(R::ok(out)))
}

/// `POST /upload-sessions/{session_id}/files:remove` → 200 OK
pub async fn remove_files(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(session_id): Path<String>,
    Json(req): Json<RemoveFilesIn>,
) -> Result<Json<R<RemoveFilesOut>>, AppError> {
    let out = UploadSessionService::remove_files(
        state.upload_session_repo.clone(),
        state.cos.clone(),
        state.config.upload_session.ttl_seconds,
        &session_id,
        &req,
        &current,
    )
    .await?;
    Ok(Json(R::ok(out)))
}

/// `POST /upload-sessions/{session_id}/renew` → 200 OK
pub async fn renew(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(session_id): Path<String>,
    Json(req): Json<RenewIn>,
) -> Result<Json<R<RenewOut>>, AppError> {
    let out = UploadSessionService::renew(
        state.upload_session_repo.clone(),
        state.python_sts.clone(),
        &state.config.upload_session,
        &session_id,
        &req,
        &current,
    )
    .await?;
    Ok(Json(R::ok(out)))
}

/// `POST /upload-sessions/{session_id}/consume` → 200 OK
pub async fn consume_files(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(session_id): Path<String>,
    Json(req): Json<ConsumeFilesIn>,
) -> Result<Json<R<ConsumeFilesOut>>, AppError> {
    let out = UploadSessionService::consume_files(
        state.upload_session_repo.clone(),
        state.config.upload_session.ttl_seconds,
        &session_id,
        &req,
        &current,
    )
    .await?;
    Ok(Json(R::ok(out)))
}

/// `POST /upload-sessions/{session_id}/discard` → 200 OK
pub async fn discard(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(session_id): Path<String>,
    Json(req): Json<DiscardIn>,
) -> Result<Json<R<DiscardOut>>, AppError> {
    let out = UploadSessionService::discard(
        state.upload_session_repo.clone(),
        &session_id,
        &req,
        &current,
    )
    .await?;
    Ok(Json(R::ok(out)))
}

/// upload_session 域 axum 子路由。
///
/// 2026-09-18 新增。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/get-or-create", post(get_or_create))
        .route("/{session_id}/files:allocate", post(allocate_files))
        .route(
            "/{session_id}/files/{client_ref}/complete",
            post(complete_file),
        )
        .route("/{session_id}/files:remove", post(remove_files))
        .route("/{session_id}/renew", post(renew))
        .route("/{session_id}/consume", post(consume_files))
        .route("/{session_id}/discard", post(discard))
}
