//! upload_session 域 HTTP handler（2026-09-18 新增 + 2026-09-22 标注三形态）
//!
//! 7 个 POST handler，全部挂在 `/api/v2/upload-sessions`：
//! - `POST /get-or-create`                                —— 拿 / 创建上传会话
//! - `POST /{session_id}/files:allocate`                  —— 批量分配 tmp_key
//! - `POST /{session_id}/files/{client_ref}/complete`     —— head 校验 + 标 done
//! - `POST /{session_id}/files:remove`                    —— 移除条目 + 异步删 tmp
//! - `POST /{session_id}/renew`                           —— 重签 STS
//! - `POST /{session_id}/consume`                         —— 仅移除条目（tmp 删由 confirm/batch 负责）
//! - `POST /{session_id}/discard`                         —— 整条 DEL
//!
//! ## 事务分层与 handler 三形态标记（2026-09-22 标注）
//!
//! **本域不开 PG tx**——handler 直接调 service，service 走 Redis（不是 Postgres）。
//! 与 `_e2e` 域同列 CLAUDE.md §约定层「handler 不开 PG tx」例外清单。
//!
//! 本域 7 个端点的 handler 三形态分类（与 iam 范本同形，但「commit」对应「post-Redis 写入」）：
//!
//! - 形态 ② 写 + post-commit 副作用（COS async spawn / STS renew）：
//!   - `get_or_create` —— Redis SET + 必要时调 `python_sts.get_credentials`（handler 无 post-commit
//!     副作用，service 自管 Redis + STS 调用；handler 仅直接调 service 方法）。
//!   - `allocate_files` —— Redis HSET + EXPIRE（纯 Redis，无 COS / STS）。
//!   - `complete_file` —— Redis HSET + spawn 异步 `cos.delete_object(tmp_key)` 兜底
//!     （清理 `tmp_placeholder` 模式遗留的 cos 对象）。post-spawn 走 handler 内
//!     `tokio::spawn` 包裹。
//!   - `remove_files` —— Redis HDEL + spawn 异步 `cos.delete_object(tmp_key)` 兜底。
//!   - `renew` —— Redis GET + EXPIRE + 必要时调 `python_sts.get_credentials` 重签 STS。
//!   - `consume_files` —— Redis HDEL（无 COS / STS）。
//!   - `discard` —— Redis DEL（无 COS / STS）。
//!
//! 注：本域所有 7 个 handler 都是形态 ② 风格（写 Redis / 调 STS / spawn COS），
//! 没有形态 ①（纯写、handler commit 后无副作用）与形态 ③（读端点、走
//! `pool.acquire()`）。读端点语义在此域也不存在——上传会话本身就是「写会话」集合。
//!
//! ## 与 service 的依赖关系（2026-09-22 不变）
//! service 形参保留原状（`Arc<dyn UploadSessionRepo>` + `Arc<dyn PythonSts>` +
//! `Arc<dyn CosClient>` + `&UploadSessionConfig`）；handler 直接传入
//! `state.upload_session_repo` / `state.python_sts` / `state.cos` /
//! `state.config.upload_session`，无中间 service 字段装线。
//!
//! ## 不动 trait `UploadSessionRepo`（已存在 3 实现）
//! - `RedisUploadSessionRepo`（生产） / `NoopUploadSessionRepo`（无 Redis 环境占位）
//!   / `InMemoryUploadSessionRepo`（测试用）。
//! - 不动 `service_tests/tests.rs`（1003 行 IO 单测保留原状）。
//!
//! ## 不动 service/mod.rs（1003 行 IO 单测是工作量但不是改造目标）
//! - `Arc<dyn UploadSessionRepo>` + `Arc<dyn PythonSts>` + `Arc<dyn CosClient>` +
//!   `&UploadSessionConfig` 形参保留原状。
//! - handler 直接调 service 方法，service 不知 PG tx（也无需知）。
//!
//! ## 约束
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（统一 `require_role(Manager + Clerk)`）。
//! - WS 广播：本域不上报 WS 事件。

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
