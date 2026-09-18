//! upload_session 域业务逻辑
//!
//! 2026-09-18 新增。
//!
//! 7 个端点对应的 service 函数，通过 `Arc<dyn UploadSessionRepo>` 操作 Redis，
//! 通过 `Arc<dyn PythonSts>` 转发 python 后端签发 STS。
//!
//! ## 关键不变式
//! - **scope 白名单硬编码**：仅 `"parts_new"`；其它 → 422 `BIZ_UPLOAD_SESSION_SCOPE_INVALID`。
//! - **session_id 防混淆**：path `session_id` 必须等于 `UploadSession.session_id`，
//!   否则 → 409 `BIZ_UPLOAD_SESSION_MISMATCH`（防 cross-user / cross-scope 误用）。
//! - **凭证自动续期**：`get_or_create` / `renew` 路径在剩余 < 600s 时自动 renew，
//!   并 EXPIRE 续期 Redis key TTL。
//! - **tmp_key 派生规则**：`{tmp_prefix}{sha16_or_nohash}_{safe_filename}`，
//!   sha16 = sha256 前 16 hex chars（同 part_file::service::bind_uploaded_file 模板）。
//! - **CAS 幂等**：allocate 时 `client_ref` 已存在 → 复用旧条目，不重新分配 tmp_key。
//! - **complete head 校验**：`cos.head_object(tmp_key)` 校验 tmp 存在 + 可选 size 一致；
//!   失败 → status=error + 错误原因附 message。
//! - **remove 异步删 tmp**：service 层 spawn `cos.delete_object` 兜底，失败仅 warn。
//! - **竞态保证**：单 key 写者即单 user（`upload_session:{user_id}:{scope}` 隔离），
//!   用最简的 GET → modify → SET EX 流程；如需更强保证可后续切 WATCH/MULTI/EXEC。

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use super::dto::{
    AllocateFileItemOut, AllocateFilesIn, AllocateFilesOut, CompleteFileIn, CompleteFileOut,
    ConsumeFilesIn, ConsumeFilesOut, DiscardIn, DiscardOut, GetOrCreateIn, GetOrCreateOut,
    RemoveFilesIn, RemoveFilesOut, RenewIn, RenewOut, SessionCredentials, SessionCredentialsOut,
    SessionFile, UploadSession, is_valid_kind, is_valid_scope, sanitize_filename,
};
use super::repo::UploadSessionRepo;
use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::python_sts::PythonSts;
use crate::shared::error::{AppError, code};

// ============================================================
// 权限守卫
// ============================================================

/// upload_session 域统一权限（与原 part_file upload-intents 一致：Manager + Clerk）。
///
/// 7 个端点全部走同一权限集合，避免前端因「少角色打不开」频繁报错。
fn require_role(current: &CurrentUser) -> Result<(), AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])
}

// ============================================================
// helper：拼 tmp_key
// ============================================================

/// 派生 tmp_key：`{tmp_prefix}{sha16}_{safe_filename}`
///
/// - `sha` 是 64 hex chars（content_sha256），取前 16 字符作为 sha16。
/// - `safe_filename` 是 sanitize_filename 处理过的字符串。
fn build_tmp_key(tmp_prefix: &str, sha: &str, original_filename: &str) -> String {
    let safe = sanitize_filename(original_filename);
    let sha16 = &sha[..16];
    // tmp_prefix 已含尾斜杠；直接拼接
    format!("{tmp_prefix}{sha16}_{safe}")
}

// ============================================================
// helper：now unix seconds
// ============================================================

fn now_unix() -> i64 {
    Utc::now().timestamp()
}

// ============================================================
// helper：签发 → 写 Redis
// ============================================================

/// 调 python 签发 + 生成完整 UploadSession 写入 Redis。
///
/// `scope` 已校验过白名单（caller 负责）。
///
/// 2026-09-18 新增。
async fn issue_and_persist(
    repo: &Arc<dyn UploadSessionRepo>,
    sts: &Arc<dyn PythonSts>,
    user_id: i64,
    scope: &str,
    ttl_seconds: u64,
    sts_duration_seconds: u32,
) -> Result<UploadSession, AppError> {
    let session_id = Uuid::new_v4().to_string();
    let tmp_prefix = format!("tmp/sess/{session_id}/");

    // 调 python 签发（不传 prefix 让 python 端规范化；当前实现是按 caller 传的 prefix 原样回传）
    let cred = sts.issue(&tmp_prefix, sts_duration_seconds).await?;

    let now = now_unix();
    let credentials = SessionCredentials {
        tmp_secret_id: cred.tmp_secret_id,
        tmp_secret_key: cred.tmp_secret_key,
        session_token: cred.session_token,
        start_time: cred.start_time,
        expired_time: cred.expired_time,
    };

    let session = UploadSession {
        session_id: session_id.clone(),
        user_id,
        scope: scope.to_string(),
        tmp_prefix: cred.tmp_prefix.clone(),
        bucket: cred.bucket,
        region: cred.region,
        expires_in: (credentials.expired_time - credentials.start_time).max(0),
        credentials,
        files: Vec::new(),
        created_at: now,
        updated_at: now,
    };

    repo.put(&session, ttl_seconds).await?;
    Ok(session)
}

/// 在已存在的 session 上更新凭证（renew 用）。
///
/// 不重新生成 session_id / tmp_prefix，保留同 session 的 tmp 前缀复用；
/// 若 python 端换 prefix 则 tmp_prefix 跟随更新（极少出现；python 端约定 prefix 稳定）。
async fn renew_credentials(
    repo: &Arc<dyn UploadSessionRepo>,
    sts: &Arc<dyn PythonSts>,
    session: &UploadSession,
    ttl_seconds: u64,
    sts_duration_seconds: u32,
) -> Result<UploadSession, AppError> {
    let cred = sts.issue(&session.tmp_prefix, sts_duration_seconds).await?;
    let now = now_unix();

    let mut updated = session.clone();
    updated.credentials = SessionCredentials {
        tmp_secret_id: cred.tmp_secret_id,
        tmp_secret_key: cred.tmp_secret_key,
        session_token: cred.session_token,
        start_time: cred.start_time,
        expired_time: cred.expired_time,
    };
    updated.expires_in = (updated.credentials.expired_time - updated.credentials.start_time).max(0);
    updated.bucket = cred.bucket;
    updated.region = cred.region;
    // tmp_prefix 跟随 python 端约定；通常与旧值一致（prefix 由 caller 拼后传入，python 端不重拼）
    updated.tmp_prefix = cred.tmp_prefix;
    updated.updated_at = now;

    repo.put(&updated, ttl_seconds).await?;
    Ok(updated)
}

// ============================================================
// 7 个 service 函数
// ============================================================

impl UploadSessionService {
    /// `POST /upload-sessions/get-or-create`（M3-B 第 1 端点）。
    ///
    /// 行为：
    /// 1. scope 白名单校验
    /// 2. miss（Redis 无 key）→ 调 python 签发 + 生成 uuid + 写 Redis
    /// 3. hit 且凭证剩余有效期 < renew_threshold_seconds → 自动 renew + 更新 Redis
    /// 4. hit 且剩余有效期足够 → 直接返回（EXPIRE 续期 TTL）
    ///
    /// 返回时**永远**保证 credentials 可用（剩余 ≥ renew_threshold）。
    ///
    /// 2026-09-18 新增。
    pub async fn get_or_create(
        repo: Arc<dyn UploadSessionRepo>,
        sts: Arc<dyn PythonSts>,
        cfg: &crate::infra::config::UploadSessionConfig,
        req: &GetOrCreateIn,
        current: &CurrentUser,
    ) -> Result<GetOrCreateOut, AppError> {
        require_role(current)?;
        if !is_valid_scope(&req.scope) {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_SCOPE_INVALID,
                format!("scope {:?} 不在白名单（首期仅 parts_new）", req.scope),
            ));
        }

        let now = now_unix();
        let session = match repo.get(current.id, &req.scope).await? {
            None => {
                issue_and_persist(
                    &repo,
                    &sts,
                    current.id,
                    &req.scope,
                    cfg.ttl_seconds,
                    cfg.sts_duration_seconds,
                )
                .await?
            }
            Some(s) => {
                // 命中：检查剩余有效期
                let remaining = s.credentials.expired_time - now;
                if remaining < cfg.renew_threshold_seconds {
                    renew_credentials(&repo, &sts, &s, cfg.ttl_seconds, cfg.sts_duration_seconds)
                        .await?
                } else {
                    // 剩余足够：仅 EXPIRE 续期
                    repo.touch(current.id, &req.scope, cfg.ttl_seconds).await?;
                    s
                }
            }
        };

        Ok(GetOrCreateOut {
            session_id: session.session_id.clone(),
            scope: session.scope.clone(),
            tmp_prefix: session.tmp_prefix.clone(),
            bucket: session.bucket.clone(),
            region: session.region.clone(),
            credentials: SessionCredentialsOut::from(&session.credentials),
            expires_in: session.expires_in,
            files: session.files.clone(),
        })
    }

    /// `POST /upload-sessions/{session_id}/files:allocate`。
    ///
    /// 行为：
    /// 1. scope 白名单 + session_id 防混淆
    /// 2. 批量逐项校验 kind（422 BAD_TYPE）
    /// 3. 幂等：client_ref 已存在 → 返回旧条目，不重新分配 tmp_key
    /// 4. 派生 tmp_key：`{tmp_prefix}{sha16}_{safe_filename}`
    /// 5. 原子覆盖写 Redis
    ///
    /// 2026-09-18 新增。
    pub async fn allocate_files(
        repo: Arc<dyn UploadSessionRepo>,
        cfg_ttl_seconds: u64,
        session_id: &str,
        req: &AllocateFilesIn,
        current: &CurrentUser,
    ) -> Result<AllocateFilesOut, AppError> {
        require_role(current)?;
        if !is_valid_scope(&req.scope) {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_SCOPE_INVALID,
                format!("scope {:?} 不在白名单", req.scope),
            ));
        }

        let mut session = repo.get(current.id, &req.scope).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_UPLOAD_SESSION_NOT_FOUND,
                "session 不存在（discarded / TTL 过期）",
            )
        })?;
        if session.session_id != session_id {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_MISMATCH,
                format!(
                    "path session_id {session_id:?} 与 Redis session.session_id {:?} 不匹配",
                    session.session_id
                ),
            ));
        }

        let mut out_items: Vec<AllocateFileItemOut> = Vec::with_capacity(req.files.len());
        for item in &req.files {
            // 幂等：client_ref 已存在 → 返回旧条目
            if let Some(existing) = session
                .files
                .iter()
                .find(|f| f.client_ref == item.client_ref)
            {
                out_items.push(AllocateFileItemOut {
                    client_ref: existing.client_ref.clone(),
                    tmp_key: existing.tmp_key.clone(),
                });
                continue;
            }
            // 校验 kind
            if !is_valid_kind(&item.kind) {
                return Err(AppError::biz(
                    code::BIZ_UPLOAD_SESSION_BAD_TYPE,
                    format!("kind {:?} 不在白名单（仅 drawing / 3d_model）", item.kind),
                ));
            }
            // 校验 sha256 长度
            super::dto::check_sha256(&item.content_sha256)?;
            // 校验 filename 非空
            if item.original_filename.is_empty() {
                return Err(AppError::validation("original_filename 不可为空"));
            }
            // 校验 file_size > 0
            if item.file_size <= 0 {
                return Err(AppError::validation("file_size 必须 > 0"));
            }

            let tmp_key = build_tmp_key(
                &session.tmp_prefix,
                &item.content_sha256,
                &item.original_filename,
            );
            let status = "pending".to_string();
            let file = SessionFile {
                client_ref: item.client_ref.clone(),
                kind: item.kind.clone(),
                original_filename: item.original_filename.clone(),
                file_size: item.file_size,
                content_type: item.content_type.clone(),
                content_sha256: item.content_sha256.clone(),
                tmp_key: tmp_key.clone(),
                status,
                etag: None,
                uploaded_at: None,
            };
            session.files.push(file);
            out_items.push(AllocateFileItemOut {
                client_ref: item.client_ref.clone(),
                tmp_key,
            });
        }
        session.updated_at = now_unix();
        repo.put(&session, cfg_ttl_seconds).await?;

        Ok(AllocateFilesOut { items: out_items })
    }

    /// `POST /upload-sessions/{session_id}/files/{client_ref}/complete`。
    ///
    /// 行为：
    /// 1. scope 白名单 + session_id 防混淆
    /// 2. 找到 client_ref 对应的 file
    /// 3. `cos.head_object(tmp_key)` 校验存在 + 可选 size 一致
    ///    - 失败 → status=error + message 含错误原因
    ///    - 成功 → status=done + etag + uploaded_at
    /// 4. 写 Redis
    ///
    /// 2026-09-18 新增。
    pub async fn complete_file(
        repo: Arc<dyn UploadSessionRepo>,
        cos: Arc<dyn CosClient>,
        cfg_ttl_seconds: u64,
        session_id: &str,
        client_ref: &str,
        req: &CompleteFileIn,
        current: &CurrentUser,
    ) -> Result<CompleteFileOut, AppError> {
        require_role(current)?;
        if !is_valid_scope(&req.scope) {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_SCOPE_INVALID,
                format!("scope {:?} 不在白名单", req.scope),
            ));
        }

        let mut session = repo.get(current.id, &req.scope).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_UPLOAD_SESSION_NOT_FOUND,
                "session 不存在（discarded / TTL 过期）",
            )
        })?;
        if session.session_id != session_id {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_MISMATCH,
                format!(
                    "path session_id {session_id:?} 与 Redis session.session_id {:?} 不匹配",
                    session.session_id
                ),
            ));
        }

        let pos = session
            .files
            .iter()
            .position(|f| f.client_ref == client_ref)
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_UPLOAD_SESSION_FILE_NOT_FOUND,
                    format!("client_ref {client_ref:?} 不在 session.files"),
                )
            })?;

        // head_object 校验
        let meta = cos.head_object(&session.files[pos].tmp_key).await;
        match meta {
            Err(e) => {
                // 标记 error 并写回
                session.files[pos].status = "error".to_string();
                let msg = format!("head_object 失败: {e}");
                // 用 etag 字段携带错误信息（契约允许 etag 为空字符串 → 序列化时省略；本处
                // 用 status=error + 返回原 file 让前端判断）
                session.updated_at = now_unix();
                repo.put(&session, cfg_ttl_seconds).await?;
                Err(AppError::biz(code::BIZ_UPLOAD_SESSION_HEAD_FAILED, msg))
            }
            Ok(m) => {
                // size 一致性校验
                if let Some(declared) = req.file_size
                    && declared != m.size
                {
                    session.files[pos].status = "error".to_string();
                    session.updated_at = now_unix();
                    repo.put(&session, cfg_ttl_seconds).await?;
                    return Err(AppError::biz(
                        code::BIZ_UPLOAD_SESSION_SIZE_MISMATCH,
                        format!("head size {} 与声明 {declared} 不一致", m.size),
                    ));
                }
                session.files[pos].status = "done".to_string();
                session.files[pos].etag = Some(m.etag.clone());
                session.files[pos].uploaded_at = Some(Utc::now().to_rfc3339());
                // 客户端声明的 etag（若有）覆盖 head 返回的 etag 时，保留 head 返回值（CAS 真值）
                // 注释：本模块契约 `etag` 是 CAS 真值来源（head_object 返回），客户端声明
                // 仅作 audit 参考；service 层忽略 req.etag。

                // size 一致情况下同步 file_size 为 head 实测值
                session.files[pos].file_size = m.size;
                session.updated_at = now_unix();
                let out = session.files[pos].clone();
                repo.put(&session, cfg_ttl_seconds).await?;
                Ok(out)
            }
        }
    }

    /// `POST /upload-sessions/{session_id}/files:remove`。
    ///
    /// 行为：
    /// 1. scope 白名单 + session_id 防混淆
    /// 2. 从 session.files 移除条目（仅真实移除的计入 `removed`）
    /// 3. spawn 异步 `cos.delete_object(tmp_key)` 兜底清理 tmp 对象
    ///    （best-effort，失败仅 warn）
    ///
    /// 2026-09-18 新增。
    pub async fn remove_files(
        repo: Arc<dyn UploadSessionRepo>,
        cos: Arc<dyn CosClient>,
        cfg_ttl_seconds: u64,
        session_id: &str,
        req: &RemoveFilesIn,
        current: &CurrentUser,
    ) -> Result<RemoveFilesOut, AppError> {
        require_role(current)?;
        if !is_valid_scope(&req.scope) {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_SCOPE_INVALID,
                format!("scope {:?} 不在白名单", req.scope),
            ));
        }

        let mut session = repo
            .get(current.id, &req.scope)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_UPLOAD_SESSION_NOT_FOUND, "session 不存在"))?;
        if session.session_id != session_id {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_MISMATCH,
                format!(
                    "path session_id {session_id:?} 与 Redis session.session_id {:?} 不匹配",
                    session.session_id
                ),
            ));
        }

        let mut removed: Vec<String> = Vec::new();
        let mut tmp_keys_to_delete: Vec<String> = Vec::new();
        for client_ref in &req.client_refs {
            if let Some(pos) = session
                .files
                .iter()
                .position(|f| &f.client_ref == client_ref)
            {
                let removed_file = session.files.remove(pos);
                removed.push(removed_file.client_ref);
                tmp_keys_to_delete.push(removed_file.tmp_key);
            }
            // 不存在的 client_ref 静默跳过（不计入 removed）
        }
        session.updated_at = now_unix();
        repo.put(&session, cfg_ttl_seconds).await?;

        // 异步删 tmp 对象（best-effort；失败仅 warn）
        let cos_for_cleanup = cos.clone();
        tokio::spawn(async move {
            for key in tmp_keys_to_delete {
                if let Err(e) = cos_for_cleanup.delete_object(&key).await {
                    tracing::warn!(
                        tmp_key = %key,
                        error = %e,
                        "upload_session remove_files 异步清理 tmp 失败（best-effort）"
                    );
                }
            }
        });

        Ok(RemoveFilesOut { removed })
    }

    /// `POST /upload-sessions/{session_id}/renew`。
    ///
    /// 行为：调 python 重签同 prefix，更新 session JSON 中 credentials 字段。
    ///
    /// 2026-09-18 新增。
    pub async fn renew(
        repo: Arc<dyn UploadSessionRepo>,
        sts: Arc<dyn PythonSts>,
        cfg: &crate::infra::config::UploadSessionConfig,
        session_id: &str,
        req: &RenewIn,
        current: &CurrentUser,
    ) -> Result<RenewOut, AppError> {
        require_role(current)?;
        if !is_valid_scope(&req.scope) {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_SCOPE_INVALID,
                format!("scope {:?} 不在白名单", req.scope),
            ));
        }

        let session = repo.get(current.id, &req.scope).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_UPLOAD_SESSION_NOT_FOUND,
                "session 不存在（discarded / TTL 过期）",
            )
        })?;
        if session.session_id != session_id {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_MISMATCH,
                format!(
                    "path session_id {session_id:?} 与 Redis session.session_id {:?} 不匹配",
                    session.session_id
                ),
            ));
        }

        let updated = renew_credentials(
            &repo,
            &sts,
            &session,
            cfg.ttl_seconds,
            cfg.sts_duration_seconds,
        )
        .await?;

        Ok(RenewOut {
            credentials: SessionCredentialsOut::from(&updated.credentials),
            expires_in: updated.expires_in,
        })
    }

    /// `POST /upload-sessions/{session_id}/consume`。
    ///
    /// 行为：从 session.files 移除条目（**不**触发 tmp 删——理由见 batch.rs 现有
    /// spawn delete_object 由 confirm / batch_create 端点统一负责）。
    ///
    /// 2026-09-18 新增。
    pub async fn consume_files(
        repo: Arc<dyn UploadSessionRepo>,
        cfg_ttl_seconds: u64,
        session_id: &str,
        req: &ConsumeFilesIn,
        current: &CurrentUser,
    ) -> Result<ConsumeFilesOut, AppError> {
        require_role(current)?;
        if !is_valid_scope(&req.scope) {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_SCOPE_INVALID,
                format!("scope {:?} 不在白名单", req.scope),
            ));
        }

        let mut session = repo
            .get(current.id, &req.scope)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_UPLOAD_SESSION_NOT_FOUND, "session 不存在"))?;
        if session.session_id != session_id {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_MISMATCH,
                format!(
                    "path session_id {session_id:?} 与 Redis session.session_id {:?} 不匹配",
                    session.session_id
                ),
            ));
        }

        let mut consumed: Vec<String> = Vec::new();
        for client_ref in &req.client_refs {
            if let Some(pos) = session
                .files
                .iter()
                .position(|f| &f.client_ref == client_ref)
            {
                let removed = session.files.remove(pos);
                consumed.push(removed.client_ref);
            }
        }
        session.updated_at = now_unix();
        repo.put(&session, cfg_ttl_seconds).await?;

        Ok(ConsumeFilesOut { consumed })
    }

    /// `POST /upload-sessions/{session_id}/discard`。
    ///
    /// 行为：直接 DEL Redis key 整条（**不**清理 tmp 对象——业务上调用方已通过
    /// complete/confirm 链路把 tmp → CAS key，tmp 已被 consume 链路或 confirm 后
    /// 异步 delete 清理；此处仅删 session 元数据）。
    ///
    /// 2026-09-18 新增。
    pub async fn discard(
        repo: Arc<dyn UploadSessionRepo>,
        session_id: &str,
        req: &DiscardIn,
        current: &CurrentUser,
    ) -> Result<DiscardOut, AppError> {
        require_role(current)?;
        if !is_valid_scope(&req.scope) {
            return Err(AppError::biz(
                code::BIZ_UPLOAD_SESSION_SCOPE_INVALID,
                format!("scope {:?} 不在白名单", req.scope),
            ));
        }

        // 即使 Redis key 已不存在（重复 discard），也允许 idempotent 操作；
        // 但仍要校验 scope + session_id 防误用
        let session = repo.get(current.id, &req.scope).await?;
        match session {
            None => {
                // 第一次 discard 后 TTL 未过期前已删 → 幂等返回 session_id
                // 重复 discard → 返回 "session_id 不存在" 也安全（caller 不依赖该字段业务值）
                Ok(DiscardOut {
                    session_id: session_id.to_string(),
                })
            }
            Some(s) => {
                if s.session_id != session_id {
                    return Err(AppError::biz(
                        code::BIZ_UPLOAD_SESSION_MISMATCH,
                        format!(
                            "path session_id {session_id:?} 与 Redis session.session_id {:?} 不匹配",
                            s.session_id
                        ),
                    ));
                }
                let _ = repo.delete(current.id, &req.scope).await?;
                Ok(DiscardOut {
                    session_id: s.session_id,
                })
            }
        }
    }
}

/// upload_session 域 service namespace。
///
/// 2026-09-18 新增。
pub struct UploadSessionService;

// 占位 trait；service 当前所有方法走 free function 形式（避免 `UploadSessionService` 自身
// 类型在 handler 注入时变得冗长）。保留空 trait 仅供未来扩展（如将 free fn 改方法链）。
#[async_trait]
pub trait _UploadSessionServiceMarker: Send + Sync {}

#[cfg(test)]
mod tests {
    use super::super::dto::{
        AllocateFileItemIn, ConsumeFilesIn, DiscardIn, RemoveFilesIn, RenewIn,
    };
    use super::super::repo::InMemoryUploadSessionRepo;
    use super::*;
    use crate::auth::rbac::{CurrentUser, Role};
    use crate::infra::cos::NoopCos;
    use crate::infra::python_sts::{NoopPythonSts, PythonSts, PythonStsCredential};

    fn current_user() -> CurrentUser {
        CurrentUser {
            id: 42,
            username: "u".into(),
            roles: vec![Role::Manager],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    fn current_user_clerk() -> CurrentUser {
        CurrentUser {
            id: 42,
            username: "u".into(),
            roles: vec![Role::Clerk],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    fn current_user_inspector() -> CurrentUser {
        CurrentUser {
            id: 42,
            username: "u".into(),
            roles: vec![Role::Inspector],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    fn cfg() -> crate::infra::config::UploadSessionConfig {
        crate::infra::config::UploadSessionConfig {
            python_backend_base_url: "http://x".into(),
            ttl_seconds: 86400,
            sts_duration_seconds: 7200,
            renew_threshold_seconds: 600,
        }
    }

    fn dummy_sts() -> Arc<dyn PythonSts> {
        // 总是返回 3600s 过期的占位（get_or_create 默认 600s 阈值会触发 renew）
        Arc::new(NoopPythonSts)
    }

    fn dummy_sts_long_expiry() -> Arc<dyn PythonSts> {
        struct LongSts;
        #[async_trait]
        impl PythonSts for LongSts {
            async fn issue(
                &self,
                prefix: &str,
                _expire_seconds: u32,
            ) -> Result<PythonStsCredential, AppError> {
                let now = now_unix();
                Ok(PythonStsCredential {
                    tmp_secret_id: "id".into(),
                    tmp_secret_key: "key".into(),
                    session_token: "tok".into(),
                    start_time: now,
                    expired_time: now + 86400, // 24h，远超 600s 阈值
                    bucket: "b".into(),
                    region: "r".into(),
                    tmp_prefix: prefix.into(),
                })
            }
        }
        Arc::new(LongSts)
    }

    #[tokio::test]
    async fn get_or_create_miss_path_writes_redis() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());
        let out = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .expect("miss 路径应成功");

        assert!(!out.session_id.is_empty());
        assert_eq!(out.scope, "parts_new");
        assert!(out.tmp_prefix.ends_with('/'));
        assert_eq!(out.files.len(), 0);

        // 验证 Redis 真的写入了
        let stored = repo
            .get(42, "parts_new")
            .await
            .unwrap()
            .expect("must exist");
        assert_eq!(stored.session_id, out.session_id);
    }

    #[tokio::test]
    async fn get_or_create_hit_with_long_expiry_does_not_renew() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        // 第一次创建（long expiry，24h）
        let out1 = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let cred1_session_id = out1.session_id.clone();

        // 第二次调用（命中；long expiry 24h 不触发 renew）
        let out2 = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();

        // session_id 不变（hit 路径未走 renew）
        assert_eq!(out2.session_id, cred1_session_id);
    }

    #[tokio::test]
    async fn get_or_create_hit_with_short_expiry_triggers_renew() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        // 第一次：NoopPythonSts 1h 凭证（剩余 3600s > 600s 阈值，**不**renew）
        let out1 = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let cred1_expired = out1.credentials.expired_time;

        // 篡改 session 让凭证"看起来快过期"——直接 mutate Redis 模拟
        // 把 expired_time 改成「之前很久」保证 renew 后的新凭证明显更新。
        {
            let mut s = repo.get(42, "parts_new").await.unwrap().unwrap();
            s.credentials.expired_time = 1_000_000; // 1970 年，远小于 renew 阈值
            repo.put(&s, 86400).await.unwrap();
        }

        // 第二次：触发 renew
        let out2 = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();

        // session_id 保持；expired_time 已被 renew
        // - 之前是被篡改到 1_000_000（epoch 1970+）
        // - renew 后 NoopPythonSts 返回 now+3600（远大于 1_000_000）
        // - 与 cred1_expired（同一次测试中第一次 get_or_create 的 expired_time，
        //   也在 now+3600 附近）不一定严格 >（同一秒内相同），但**绝不会** < 1_000_000。
        assert_eq!(out2.session_id, out1.session_id);
        assert!(
            out2.credentials.expired_time >= 1_000_000,
            "renew 后 expired_time 已被 python 重签（应至少回到正常 unix 秒级），got {}",
            out2.credentials.expired_time,
        );
        // 关键：被篡改到 1_000_000 后，renew 应"挽救"凭证回正常时间。
        // 用 out1 凭证做参照（同一测试里时序紧密，now 几乎一致）：
        assert!(
            (out2.credentials.expired_time - cred1_expired).abs() <= 3600,
            "renew 后的 expired_time 应与 out1 同量级（NoopSts 都是 now+3600），got diff={}",
            out2.credentials.expired_time as i64 - cred1_expired as i64,
        );
    }

    #[tokio::test]
    async fn scope_whitelist_rejects() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());
        let err = UploadSessionService::get_or_create(
            repo,
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "wrong_scope".into(),
            },
            &current_user(),
        )
        .await
        .expect_err("scope 不在白名单应报错");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_SCOPE_INVALID);
    }

    #[tokio::test]
    async fn session_id_mismatch_returns_409() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        // 创建 session
        UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();

        // 用错误的 session_id 调 allocate
        let err = UploadSessionService::allocate_files(
            repo,
            86400,
            "wrong-session-id",
            &AllocateFilesIn {
                scope: "parts_new".into(),
                files: vec![AllocateFileItemIn {
                    client_ref: "r1".into(),
                    kind: "drawing".into(),
                    original_filename: "a.pdf".into(),
                    file_size: 1024,
                    content_type: "application/pdf".into(),
                    content_sha256: "a".repeat(64),
                }],
            },
            &current_user(),
        )
        .await
        .expect_err("session_id 不匹配应报错");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_MISMATCH);
    }

    #[tokio::test]
    async fn allocate_idempotent_returns_existing_tmp_key() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        let out1 = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let session_id = out1.session_id.clone();

        let item = AllocateFileItemIn {
            client_ref: "r1".into(),
            kind: "drawing".into(),
            original_filename: "drawing.pdf".into(),
            file_size: 1024,
            content_type: "application/pdf".into(),
            content_sha256: "a".repeat(64),
        };
        let allocate_req = AllocateFilesIn {
            scope: "parts_new".into(),
            files: vec![item.clone()],
        };

        // 第一次 allocate
        let out_a = UploadSessionService::allocate_files(
            repo.clone(),
            86400,
            &session_id,
            &allocate_req,
            &current_user(),
        )
        .await
        .unwrap();
        assert_eq!(out_a.items.len(), 1);
        let first_tmp_key = out_a.items[0].tmp_key.clone();

        // 第二次 allocate（client_ref 相同）→ 幂等
        let out_b = UploadSessionService::allocate_files(
            repo.clone(),
            86400,
            &session_id,
            &allocate_req,
            &current_user(),
        )
        .await
        .unwrap();
        assert_eq!(out_b.items[0].tmp_key, first_tmp_key);
        // session.files 仍只 1 条
        let stored = repo.get(42, "parts_new").await.unwrap().unwrap();
        assert_eq!(stored.files.len(), 1);
    }

    #[tokio::test]
    async fn allocate_tmp_key_layout() {
        // 派生规则：`{tmp_prefix}{sha16}_{safe_filename}`
        // sha "a".repeat(64) → "a" * 16
        // safe_filename("图纸 v2.pdf") = "___v2.pdf"（3 个 _）
        // 加上中间分隔符 `_` → `aaaa_` + `___v2.pdf` = 4 个 _ 连续
        let key = build_tmp_key("tmp/sess/abc/", &"a".repeat(64), "图纸 v2.pdf");
        assert_eq!(key, "tmp/sess/abc/aaaaaaaaaaaaaaaa____v2.pdf");
    }

    #[tokio::test]
    async fn complete_head_succeeds_marks_done() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        let out = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let session_id = out.session_id.clone();

        UploadSessionService::allocate_files(
            repo.clone(),
            86400,
            &session_id,
            &AllocateFilesIn {
                scope: "parts_new".into(),
                files: vec![AllocateFileItemIn {
                    client_ref: "r1".into(),
                    kind: "drawing".into(),
                    original_filename: "a.pdf".into(),
                    file_size: 1024,
                    content_type: "application/pdf".into(),
                    content_sha256: "a".repeat(64),
                }],
            },
            &current_user(),
        )
        .await
        .unwrap();

        // NoopCos::head_object 返回 size=0, etag="noop"；本测试不传 file_size → 跳过 size 校验
        let res = UploadSessionService::complete_file(
            repo.clone(),
            Arc::new(NoopCos),
            86400,
            &session_id,
            "r1",
            &CompleteFileIn {
                scope: "parts_new".into(),
                etag: None,
                file_size: None,
            },
            &current_user(),
        )
        .await
        .expect("NoopCos head 不抛错");
        assert_eq!(res.status, "done");
        assert_eq!(res.etag.as_deref(), Some("noop"));
        assert!(res.uploaded_at.is_some());
    }

    #[tokio::test]
    async fn complete_size_mismatch_marks_error() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        let out = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let session_id = out.session_id.clone();

        UploadSessionService::allocate_files(
            repo.clone(),
            86400,
            &session_id,
            &AllocateFilesIn {
                scope: "parts_new".into(),
                files: vec![AllocateFileItemIn {
                    client_ref: "r1".into(),
                    kind: "drawing".into(),
                    original_filename: "a.pdf".into(),
                    file_size: 1024,
                    content_type: "application/pdf".into(),
                    content_sha256: "a".repeat(64),
                }],
            },
            &current_user(),
        )
        .await
        .unwrap();

        // NoopCos::head_object 返回 size=0；声明 999 → 不一致 → 21107
        let err = UploadSessionService::complete_file(
            repo,
            Arc::new(NoopCos),
            86400,
            &session_id,
            "r1",
            &CompleteFileIn {
                scope: "parts_new".into(),
                etag: None,
                file_size: Some(999),
            },
            &current_user(),
        )
        .await
        .expect_err("size 不一致应报错");
        assert_eq!(err.code(), code::BIZ_UPLOAD_SESSION_SIZE_MISMATCH);
    }

    #[tokio::test]
    async fn remove_files_drops_entries() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        let out = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let session_id = out.session_id.clone();

        UploadSessionService::allocate_files(
            repo.clone(),
            86400,
            &session_id,
            &AllocateFilesIn {
                scope: "parts_new".into(),
                files: vec![
                    AllocateFileItemIn {
                        client_ref: "r1".into(),
                        kind: "drawing".into(),
                        original_filename: "a.pdf".into(),
                        file_size: 1024,
                        content_type: "application/pdf".into(),
                        content_sha256: "a".repeat(64),
                    },
                    AllocateFileItemIn {
                        client_ref: "r2".into(),
                        kind: "drawing".into(),
                        original_filename: "b.pdf".into(),
                        file_size: 2048,
                        content_type: "application/pdf".into(),
                        content_sha256: "b".repeat(64),
                    },
                ],
            },
            &current_user(),
        )
        .await
        .unwrap();

        let out_remove = UploadSessionService::remove_files(
            repo.clone(),
            Arc::new(NoopCos),
            86400,
            &session_id,
            &RemoveFilesIn {
                scope: "parts_new".into(),
                client_refs: vec!["r1".into(), "not_exist".into()],
            },
            &current_user(),
        )
        .await
        .unwrap();
        // 仅真实移除的计入
        assert_eq!(out_remove.removed, vec!["r1".to_string()]);

        let stored = repo.get(42, "parts_new").await.unwrap().unwrap();
        assert_eq!(stored.files.len(), 1);
        assert_eq!(stored.files[0].client_ref, "r2");
    }

    #[tokio::test]
    async fn discard_deletes_redis_key() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        let out = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let session_id = out.session_id.clone();

        let out_discard = UploadSessionService::discard(
            repo.clone(),
            &session_id,
            &DiscardIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        assert_eq!(out_discard.session_id, session_id);

        // Redis 已删
        assert!(repo.get(42, "parts_new").await.unwrap().is_none());

        // 重复 discard：幂等返回（不报错）
        let _ = UploadSessionService::discard(
            repo.clone(),
            &session_id,
            &DiscardIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn consume_files_drops_without_cos_delete() {
        // 与 remove_files 区别：consume 不触发 cos.delete_object（避免与
        // batch.rs 的 spawn delete_object 重复；consume 业务语义是"已消费"，
        // tmp 删理由由 confirm / batch_create 端点统一处理）。
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        let out = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let session_id = out.session_id.clone();

        UploadSessionService::allocate_files(
            repo.clone(),
            86400,
            &session_id,
            &AllocateFilesIn {
                scope: "parts_new".into(),
                files: vec![AllocateFileItemIn {
                    client_ref: "r1".into(),
                    kind: "drawing".into(),
                    original_filename: "a.pdf".into(),
                    file_size: 1024,
                    content_type: "application/pdf".into(),
                    content_sha256: "a".repeat(64),
                }],
            },
            &current_user(),
        )
        .await
        .unwrap();

        let res = UploadSessionService::consume_files(
            repo.clone(),
            86400,
            &session_id,
            &ConsumeFilesIn {
                scope: "parts_new".into(),
                client_refs: vec!["r1".into()],
            },
            &current_user(),
        )
        .await
        .unwrap();
        assert_eq!(res.consumed, vec!["r1".to_string()]);
        let stored = repo.get(42, "parts_new").await.unwrap().unwrap();
        assert_eq!(stored.files.len(), 0);
    }

    #[tokio::test]
    async fn renew_returns_new_credentials() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());

        let out1 = UploadSessionService::get_or_create(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        let session_id = out1.session_id.clone();
        let cred1_expired = out1.credentials.expired_time;

        let renew_out = UploadSessionService::renew(
            repo.clone(),
            dummy_sts_long_expiry(),
            &cfg(),
            &session_id,
            &RenewIn {
                scope: "parts_new".into(),
            },
            &current_user(),
        )
        .await
        .unwrap();
        assert!(renew_out.credentials.expired_time >= cred1_expired);
    }

    #[tokio::test]
    async fn clerk_role_also_allowed() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());
        let out = UploadSessionService::get_or_create(
            repo,
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user_clerk(),
        )
        .await
        .expect("Clerk 应允许");
        assert!(!out.session_id.is_empty());
    }

    #[tokio::test]
    async fn inspector_role_is_rejected() {
        let repo = Arc::new(InMemoryUploadSessionRepo::new());
        let err = UploadSessionService::get_or_create(
            repo,
            dummy_sts_long_expiry(),
            &cfg(),
            &GetOrCreateIn {
                scope: "parts_new".into(),
            },
            &current_user_inspector(),
        )
        .await
        .expect_err("Inspector 应被拒");
        assert_eq!(err.code(), code::FORBIDDEN);
    }
}
