//! upload_session 域业务逻辑
//!
//! 2026-09-18 新增。
//!
//! 7 个端点对应的 service 函数，通过 `Arc<dyn UploadSessionRepo>` 操作 Redis，
//! 通过 `Arc<dyn PythonSts>` 转发 python 后端签发 STS。
//!
//! ## 文件结构（2026-09-18 review #1 修复）
//! - `mod.rs`（本文件）：helpers + 7 个端点 service 函数 + `UploadSessionService` 结构
//!   + 空 trait 占位（~666 行，不含 tests 块）
//! - `tests.rs`：service 层单元测试（按仓库 `mod tests` 模式外挂为独立文件；
//!   按 `docs/conventions.md §2` 测试不计入 1000 行硬红线）
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

#[cfg(test)]
mod tests;

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
///
/// 2026-09-18 review #10 修复：原代码 `&sha[..16]` 在 `sha.len() < 16` 时会
/// byte-index panic；现在加防御性 assert 给出明确错误信息（理论上
/// `dto::check_sha256` 已在 allocate_files 入口校验 64 hex，但 helper
/// 是公共的，外部调用也需守护）。
fn build_tmp_key(tmp_prefix: &str, sha: &str, original_filename: &str) -> String {
    assert!(
        sha.len() >= 16,
        "build_tmp_key: sha 至少 16 hex chars（实际 {}），调用方应先调 dto::check_sha256",
        sha.len()
    );
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
/// 2026-09-18 新增；2026-09-18 第 2 轮 review 修复：`PythonStsCredential` 字段路径
/// 调整（嵌套 `credentials` + 顶层另 7 字段）；`tmp_prefix` 不再由 python 返回，
/// 用本地变量（caller 已传入）作为 session.tmp_prefix。
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

    // 调 python 签发。python schema 不返回 tmp_prefix（按 caller 传入原样回写
    // 由 python 端业务约定；rust 端不二次解析），tmp_prefix 由本地变量持有。
    let cred = sts.issue(&tmp_prefix, sts_duration_seconds).await?;

    let now = now_unix();
    let credentials = SessionCredentials {
        tmp_secret_id: cred.credentials.tmp_secret_id,
        tmp_secret_key: cred.credentials.tmp_secret_key,
        session_token: cred.credentials.session_token,
        start_time: cred.credentials.start_time,
        expired_time: cred.credentials.expired_time,
    };

    let session = UploadSession {
        session_id: session_id.clone(),
        user_id,
        scope: scope.to_string(),
        // tmp_prefix 由 caller 拼好传给 python；不再从 cred 回读（python schema
        // 不返回该字段）。renew_credentials 同样保留 session.tmp_prefix 不动。
        tmp_prefix: tmp_prefix.clone(),
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
/// 不重新生成 session_id / tmp_prefix，保留同 session 的 tmp 前缀复用（caller
/// 把 session.tmp_prefix 传给 python，python 端约定原样回传；rust 端不再从 cred
/// 回读 tmp_prefix）。
///
/// 2026-09-18 review #13 修复（prefix 漂移守护）的核心不变量保持：session.tmp_prefix
/// 在 renew 前后**不变**——若 python 端异常漂移，由后续使用 `tmp_prefix` 派生的
/// `session.files[*].tmp_key` 找不到对应 COS 对象来暴露。本函数不再做 prefix 比对
/// 报错（python schema 不返回 tmp_prefix，无法比对），改由 service 整体契约
/// 保证 tmp_prefix 由 rust 端独占持有。
async fn renew_credentials(
    repo: &Arc<dyn UploadSessionRepo>,
    sts: &Arc<dyn PythonSts>,
    session: &UploadSession,
    ttl_seconds: u64,
    sts_duration_seconds: u32,
) -> Result<UploadSession, AppError> {
    // 复用 session.tmp_prefix 传给 python；rust 端独占持有 tmp_prefix 派生权。
    let cred = sts.issue(&session.tmp_prefix, sts_duration_seconds).await?;
    let now = now_unix();

    let mut updated = session.clone();
    updated.credentials = SessionCredentials {
        tmp_secret_id: cred.credentials.tmp_secret_id,
        tmp_secret_key: cred.credentials.tmp_secret_key,
        session_token: cred.credentials.session_token,
        start_time: cred.credentials.start_time,
        expired_time: cred.credentials.expired_time,
    };
    updated.expires_in = (updated.credentials.expired_time - updated.credentials.start_time).max(0);
    updated.bucket = cred.bucket;
    updated.region = cred.region;
    // tmp_prefix 保持 session 旧值（不来自 cred；2026-09-18 第 2 轮 review 修复）
    updated.tmp_prefix = session.tmp_prefix.clone();
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
    /// 2026-09-18 review #14 修复：原代码 Redis key 不存在 → 直接返回 caller 传入
    /// 的 session_id（幂等），但**不校验 session_id 一致性**——若 caller 误传了
    /// 别的 session_id 而 Redis 里那条已 TTL 过期，会静默返回成功，造成跨用户
    /// 误用不可见。新代码：Redis 不存在时也要求 caller 在另一处能校验（如果 session
    /// 存在则必然校验 session_id 一致性；如果不存在则放弃校验，但需要给前端明确
    /// 提示——这里采取"如果 Redis key 不存在，返回 NOT_FOUND 让前端重试或检查"）。
    /// **最终决策**：保留原幂等语义，但**当 Redis 存在时强校验 session_id**。
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

        // 2026-09-18 review #14：幂等语义保留（Redis 不存在 → 直接返回 caller 传入
        // 的 session_id），但当 Redis 存在时必须校验 session.session_id 与 caller
        // 传入一致——否则 409 MISMATCH。这与 allocate / remove / consume 的处理
        // 完全一致。
        let session = repo.get(current.id, &req.scope).await?;
        match session {
            None => {
                // 幂等：Redis key 已不存在（discarded / TTL 过期），返回 caller 传入的
                // session_id。无跨用户歧义风险（Redis 已无记录），重复 discard 不报错。
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
