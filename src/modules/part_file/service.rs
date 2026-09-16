//! part_file 域业务逻辑（2026-09-14 Phase 3 + 2026-09-16 M2-B 业务层）
//!
//! 对应 Python myERP/service/part_file_service.py + service/_file_kind_policy.py。
//!
//! ## 上传核心逻辑（`upload_file_for_owner`）
//! 1. 取 `owner_kind`（PART / ASSEMBLY），校验存在性（21105 OWNER_NOT_FOUND）
//! 2. 校验 `kind` 在白名单内（policy::allowed_exts）
//! 3. 校验扩展名（policy::ext_of）—— 无扩展名 / 不在白名单 → 21102 BAD_TYPE
//! 4. 校验 content_type（policy::expected_content_types_for_ext）—— 不匹配 → 21102
//! 5. SHA-256 算 hash → 同 owner + kind + sha 撞唯一索引 → 21108 DUPLICATE（CAS 去重）
//!    注意：CAS 命中时**跳过 COS PUT**，直接复用已有 object_key（节省 COS 流量）
//! 6. 上传 COS（infra/cos.rs::put_object）
//! 7. INSERT t_part_file
//! 8. 返回 PartFileOut
//!
//! ## multipart 上传契约
//! 客户端发 multipart form-data：
//!   - `data` JSON 字段：`{"owner_kind":"PART","owner_id":"1001","kind":"DRAWING"}`
//!   - `file` 二进制字段：PDF / STEP / STL 等
//!
//! `content_type` 取客户端声明（multipart 头），扩展名从 `filename` 提取。
//!
//! ## 与 part 域的耦合
//! 装配体 PDF（kind='ASSEMBLY_MASTER'）走相同上传通道，owner_kind='ASSEMBLY'，
//! 由 assembly 域在创建流程或单独的 `POST /assemblies/{id}/files` 端点调用。
//!
//! ## 2026-09-16 M2-B 直传 COS 链路
//! - `upload_intents`：场景 A/B 一次性签发 STS + 预生成 tmp_key / CAS 去重命中复用
//! - `bind_uploaded_file`：confirm handler + batch_create service 共享的"已上传到 tmp
//!   区 → 绑定到 owner"逻辑；head/copy 在 tx 之外，事务内只做 soft_delete + INSERT

use std::sync::Arc;

use sqlx::{PgConnection, PgPool};

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part_file::dto::{
    validate, PartFileListOut, PartFileListQuery, PartFileOut, PartFileWithUrlOut,
    UploadIntentItemOut, UploadIntentsIn, UploadIntentsOut,
};
use crate::modules::part_file::model::TPartFile;
use crate::modules::part_file::policy;
use crate::modules::part_file::repo::{hash_bytes, NewPartFile, PartFileRepo};
use crate::shared::error::{code, AppError};

pub struct PartFileService;

impl PartFileService {
    /// 单文件上传（multipart `file` 字段 + JSON `data` 字段已解析）。
    ///
    /// `original_filename` 是客户端 multipart 的 filename；`content_type` 是
    /// 客户端声明的 MIME；`bytes` 是文件二进制。
    ///
    /// 返回新建的 `PartFileOut`（含 id）。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file_for_owner(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
        original_filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
        current: &CurrentUser,
    ) -> Result<PartFileOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::CncProgrammer])?;

        // 1. owner 存在性校验
        Self::assert_owner_exists(conn, owner_kind, owner_id).await?;

        // 2. kind 白名单（policy::allowed_exts 自动挡掉未知 kind）
        let ext = policy::ext_of(original_filename)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("文件缺少扩展名: {original_filename:?}")))?;
        let allowed_exts = policy::allowed_exts(kind);
        if !allowed_exts.contains(&ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("kind={kind:?} 不接受扩展名 {ext:?}"),
            ));
        }
        // 3. content_type 校验
        let expected = policy::expected_content_types_for_ext(&ext);
        if !expected.iter().any(|c| c.eq_ignore_ascii_case(content_type)) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!(
                    "kind={kind:?}, 扩展名={ext:?}: content_type {content_type:?} 不在白名单 {expected:?}"
                ),
            ));
        }
        // 4. file_type 推导
        let file_type = policy::file_type_for_ext(&ext)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("扩展名 {ext:?} 无对应 file_type")))?;

        // 5. SHA-256 → CAS 去重（撞唯一索引 → 21108 DUPLICATE）
        let sha = hash_bytes(&bytes);
        if let Some(existing) = PartFileRepo::get_by_owner_kind_sha(&mut *conn, owner_id, kind, &sha).await? {
            // CAS 命中：跳过 COS PUT，直接复用已有记录（返回 id / object_key）
            return Ok(Self::render_out(&existing, owner_kind));
        }

        // 6. 上传 COS（key 模板：`{owner_kind.to_lowercase()}/{owner_id}/{kind}/{sha}_{safe_filename}`）
        let safe_filename = sanitize_filename(original_filename);
        let object_key = format!(
            "{}/{}/{}/{}_{}",
            owner_kind.to_lowercase(),
            owner_id,
            kind,
            &sha[..16],
            safe_filename,
        );
        cos.put_object(&object_key, bytes.clone(), content_type)
            .await?;

        // 7. INSERT t_part_file
        let nf = NewPartFile {
            id: snowflake.next_id(),
            part_id: owner_id,
            owner_kind,
            kind,
            file_type,
            object_key: &object_key,
            original_filename,
            file_size: bytes.len() as i64,
            content_type,
            upload_status: "READY",
            content_sha256: Some(&sha),
            created_by: current.id,
        };
        let id = PartFileRepo::create_part_file(&mut *conn, nf).await.map_err(|e| match e {
            sqlx::Error::Database(db) => {
                if db.code().as_deref() == Some("23505") {
                    AppError::biz(
                        code::BIZ_PART_FILE_DUPLICATE,
                        format!("owner {owner_id} / {kind} / sha={} 撞唯一索引", &sha[..16]),
                    )
                } else {
                    AppError::from(sqlx::Error::Database(db))
                }
            }
            other => AppError::from(other),
        })?;

        // 8. 读回（include_deleted=true 兜底 INSERT 可见性）
        let row = PartFileRepo::get_by_id(&mut *conn, id, true)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_NOT_FOUND, "刚 INSERT 却查不到"))?;
        Ok(Self::render_out(&row, owner_kind))
    }

    /// 列表：按 owner_kind + owner_id + kind 过滤 + 分页。
    pub async fn list_files(
        conn: &mut PgConnection,
        query: &PartFileListQuery,
        current: &CurrentUser,
    ) -> Result<PartFileListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::CncProgrammer])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 500);
        let offset = query.offset.unwrap_or(0).max(0);
        let owner_kind = query.owner_kind.as_deref().unwrap_or("PART");
        let owner_id_opt: Option<i64> = query
            .owner_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<i64>().map_err(|_| {
                    AppError::biz(code::BIZ_INVALID_VALUE, format!("owner_id 非法: {s:?}"))
                })
            })
            .transpose()?;
        let kind_opt = query.kind.as_deref();

        let (rows, total) = PartFileRepo::list_with_filters(
            &mut *conn,
            owner_kind,
            owner_id_opt,
            kind_opt,
            limit,
            offset,
        )
        .await?;
        let items = rows
            .into_iter()
            .map(|r| Self::render_out(&r, owner_kind))
            .collect();
        Ok(PartFileListOut { items, total })
    }

    /// 单条详情 + 预签下载 URL。
    pub async fn get_file_with_url(
        conn: &mut PgConnection,
        cos: Arc<dyn CosClient>,
        file_id: i64,
        current: &CurrentUser,
    ) -> Result<PartFileWithUrlOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::CncProgrammer])?;
        let row = PartFileRepo::get_by_id(&mut *conn, file_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_NOT_FOUND, format!("part_file {file_id} 不存在")))?;
        let url = cos.presigned_get_url(&row.object_key, 3600).await?;
        Ok(PartFileWithUrlOut {
            id: row.id.to_string(),
            kind: row.kind,
            file_type: row.file_type,
            original_filename: row.original_filename,
            file_size: row.file_size,
            content_type: row.content_type,
            content_sha256: row.content_sha256,
            upload_status: row.upload_status,
            download_url: url,
            url_expires_in_seconds: 3600,
        })
    }

    /// 校验 owner 存在性（polymorphic）。
    async fn assert_owner_exists(
        conn: &mut PgConnection,
        owner_kind: &str,
        owner_id: i64,
    ) -> Result<(), AppError> {
        match owner_kind {
            "PART" => {
                let exists: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(owner_id)
                .fetch_optional(&mut *conn)
                .await?;
                if exists.is_none() {
                    return Err(AppError::biz(
                        code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                        format!("part {owner_id} 不存在"),
                    ));
                }
            }
            "ASSEMBLY" => {
                let exists: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM t_assembly WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(owner_id)
                .fetch_optional(&mut *conn)
                .await?;
                if exists.is_none() {
                    return Err(AppError::biz(
                        code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                        format!("assembly {owner_id} 不存在"),
                    ));
                }
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("owner_kind {other:?} 不支持（仅 PART / ASSEMBLY）"),
                ));
            }
        }
        Ok(())
    }

    /// `TPartFile` → `PartFileOut`（同时注入 owner_kind）。
    fn render_out(row: &TPartFile, owner_kind: &str) -> PartFileOut {
        PartFileOut {
            id: row.id,
            owner_id: row.part_id,
            owner_kind: owner_kind.to_string(),
            kind: row.kind.clone(),
            file_type: row.file_type.clone(),
            object_key: row.object_key.clone(),
            original_filename: row.original_filename.clone(),
            file_size: row.file_size,
            content_type: row.content_type.clone(),
            upload_status: row.upload_status.clone(),
            content_sha256: row.content_sha256.clone(),
            // 2026-09-16 补投影：CNC 配对分组契约字段（G_CODE <-> SETUP_SHEET 互指）
            paired_file_id: row.paired_file_id,
            version: row.version,
            created_at: Some(row.created_at),
            created_by: row.created_by,
        }
    }
}

/// 把 client-supplied filename 清洗为 COS object key 安全字符串：
/// - 保留 ASCII 字母 / 数字 / `.` / `-` / `_`
/// - 其它字符（含中文 / 空格）替换为 `_`
/// - 长度上限 80 字符（与 Python `re.sub(r'[^\w.-]', '_', name)[:80]` 对齐）
fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.len() > 80 {
        out.truncate(80);
    }
    if out.is_empty() {
        out.push_str("file");
    }
    out
}

// 公开给装配体 upload-files 端点用的辅助：列出某 owner 的 part_file（不限定 kind）。
#[allow(dead_code)]
pub async fn list_part_files_for_owner(
    conn: &mut PgConnection,
    owner_kind: &str,
    owner_id: i64,
) -> Result<Vec<TPartFile>, sqlx::Error> {
    PartFileRepo::list_by_owner(conn, owner_kind, owner_id).await
}

// ===== 2026-09-16 M2-B 业务层：直传 COS 链路 service =====

impl PartFileService {
    /// `POST /api/v2/part-files/upload-intents`（场景 A + 场景 B）。
    ///
    /// 流程：
    /// 1. 权限守卫（Manager + Clerk）
    /// 2. 逐项校验（kind / sha / filename / size / content_type）—— 用
    ///    [`dto::validate`] 集中函数
    /// 3. 场景 B（`owner_part_id` 非空）：
    ///    - 校验 part 存在（`assert_owner_exists`）
    ///    - 逐项查 `(part_id, kind, sha)` CAS 命中；命中 → 标 `dedup_hit=true`，
    ///      附 `existing_file`，**不分配 tmp_key**
    ///    - 未命中 → 分配 `tmp_key = format!("{owner_sub_prefix}/{kind}/{seq}_{safe}")`
    /// 4. 场景 A（`owner_part_id` 空）：
    ///    - 生成 `batch_uuid = Uuid::new_v4()`
    ///    - 逐项分配 `tmp_key = format!("{batch_sub_prefix}/{seq}_{safe}")`（**不查重**
    ///      —— part 还未建，无法查重；batch_create 时再按 (新建 part_id, kind, sha) 二次查）
    /// 5. 一次性签 STS（`state.sts.issue_for_intents(tmp_sub_prefix)`）—— 单次签发覆盖
    ///    本次 batch 的所有 tmp 对象，前端只需拿一组 credentials 即可
    /// 6. 返回 `UploadIntentsOut`
    ///
    /// 关键不变式：
    /// - **tmp_key 必须以 `tmp_sub_prefix` 开头**：前端按 prefix 写，服务端 confirm 时
    ///   按 prefix 校验（防客户端乱传 key 读到别人文件）
    /// - **STS policy resource 覆盖整个 tmp_sub_prefix**：caller 在 `issue_for_intents`
    ///   内显式构造
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_intents(
        pool: &PgPool,
        cfg_tmp_prefix: &str,
        sts: Arc<dyn crate::infra::sts::StsCredentialIssuer>,
        req: &UploadIntentsIn,
        current: &CurrentUser,
    ) -> Result<UploadIntentsOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // 全空 files 列表直接返回空 items（前端可能请求后还没勾选文件）
        if req.files.is_empty() {
            // 仍签一次 STS（保持出参形态一致；frontend 拿到 credentials 后可丢弃）
            let cred = sts.issue_for_intents(cfg_tmp_prefix).await?;
            return Ok(UploadIntentsOut {
                credentials: cos_credentials_out(&cred),
                bucket: cred.bucket,
                region: cred.region,
                tmp_prefix: cred.tmp_prefix,
                items: vec![],
            });
        }

        // 1. 逐项校验
        let max_file_size = 300 * 1024 * 1024usize; // 与 dto::validate 一致（默认 300MB）
        for item in &req.files {
            validate::check_upload_intent_item(item, max_file_size)?;
        }

        // 2. 派生 tmp_sub_prefix（场景 A 用 batch_uuid；场景 B 用 owner_part_id）
        let mut conn = pool.acquire().await?;
        let tmp_sub_prefix = if let Some(owner_id) = req.owner_part_id {
            // 场景 B：先校验 part 存在
            Self::assert_owner_exists(&mut conn, "PART", owner_id).await?;
            format!("{cfg_tmp_prefix}part/{owner_id}")
        } else {
            // 场景 A：batch_uuid 一次性生成
            format!("{cfg_tmp_prefix}{}", uuid::Uuid::new_v4())
        };

        // 3. 一次性签 STS（覆盖整 tmp_sub_prefix/*）
        let cred = sts.issue_for_intents(&tmp_sub_prefix).await?;

        // 4. 逐项分配 tmp_key 或 dedup 命中复用
        let mut items = Vec::with_capacity(req.files.len());
        for (seq, item) in req.files.iter().enumerate() {
            let client_ref = seq.to_string();
            // 场景 B：先查重；命中 → 复用，不分配 tmp_key
            if let Some(owner_id) = req.owner_part_id
                && let Some(existing) = PartFileRepo::get_by_owner_kind_sha(
                    &mut *conn,
                    owner_id,
                    &item.kind,
                    &item.content_sha256,
                )
                .await?
            {
                items.push(UploadIntentItemOut {
                    client_ref,
                    tmp_key: String::new(),
                    dedup_hit: true,
                    existing_file: Some(Self::render_out(&existing, "PART")),
                });
                continue;
            }
            // 未命中（场景 A 全走这里 + 场景 B 未命中）：分配 tmp_key
            let safe = sanitize_filename(&item.filename);
            let tmp_key = if let Some(_owner_id) = req.owner_part_id {
                // 场景 B：tmp_key 形如 `tmp/part/{owner_id}/{kind}/{seq}_{safe}`
                format!("{}/{}/{}_{}", tmp_sub_prefix, item.kind, seq, safe)
            } else {
                // 场景 A：tmp_key 形如 `tmp/{batch_uuid}/{seq}_{safe}`（无 kind 段）
                format!("{tmp_sub_prefix}/{seq}_{safe}")
            };
            items.push(UploadIntentItemOut {
                client_ref,
                tmp_key,
                dedup_hit: false,
                existing_file: None,
            });
        }

        Ok(UploadIntentsOut {
            credentials: cos_credentials_out(&cred),
            bucket: cred.bucket,
            region: cred.region,
            tmp_prefix: cred.tmp_prefix,
            items,
        })
    }

    /// 共享 service：把已上传到 COS tmp 区的一个对象绑定到 owner（INSERT t_part_file）。
    ///
    /// 调用方：`confirm handler`（T2.6）+ `batch_create service`（T2.7）。
    ///
    /// 流程：
    /// 1. `tmp_key` 前缀防呆：必须以 `cfg_tmp_prefix` 开头（防客户端乱传 key 读到别人文件）
    /// 2. `head_object` 校验对象存在 + size 与声明一致：
    ///    - 不存在 → `BIZ_PART_FILE_TMP_OBJECT_MISSING` 21114
    ///    - size 不一致 → `BIZ_PART_FILE_SIZE_MISMATCH` 21115
    /// 3. 单文件 kind（DRAWING / 3D_MODEL）：事务内先 soft_delete 旧活跃行（保留 owner+kind+deleted_at IS NULL）
    /// 4. 派生 CAS key：`util::cos_key::build_cas_key(prefix, "part", owner_id, kind, sha16, filename)`
    /// 5. `cos.copy_object(tmp_key, cas_key)` —— 走 PermanentCos（不走 STS）
    /// 6. INSERT t_part_file（upload_status="READY"）
    /// 7. 返回 `(PartFileOut, tmp_key)` —— `tmp_key` 由 caller 拿到后 spawn 异步
    ///    `delete_object(tmp_key)` 兜底清理
    ///
    /// **重要**：head/copy 是外部 IO，**不**放进事务 tx 里——tx 里只做 DB（事务回滚时
    /// IO 已发生难恢复）。顺序：先 head（tx 之外，pool 直连）→ copy（tx 之外，pool 直连）
    /// → 开 tx → soft_delete + INSERT → commit → caller spawn 异步 delete_object(tmp_key)
    /// 兜底清理。
    ///
    /// 注：`pool` 直接传（不是 `&mut tx`）—— head/copy 必须用 pool，确保与 tx 隔离。
    #[allow(clippy::too_many_arguments)]
    pub async fn bind_uploaded_file(
        pool: &PgPool,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        cfg_upload_prefix: &str,
        cfg_tmp_prefix: &str,
        owner_id: i64,
        kind: &str,
        tmp_key: &str,
        sha256: &str,
        original_filename: &str,
        file_size: i64,
        content_type: &str,
        current: &CurrentUser,
    ) -> Result<(PartFileOut, String), AppError> {
        // 1. 权限（按 kind 派生：DRAWING / 3D_MODEL → M+C；与 multipart 端点一致）
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // 2. 字段校验（kind/sha/filename/size/content_type）
        let max_file_size = 300 * 1024 * 1024usize;
        validate::check_kind(kind)?;
        validate::check_sha256(sha256)?;
        validate::check_filename(original_filename)?;
        validate::check_file_size(file_size, max_file_size)?;
        validate::check_content_type(content_type, original_filename)?;

        // 3. tmp_key 前缀防呆
        if !tmp_key.starts_with(cfg_tmp_prefix) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "tmp_key {tmp_key:?} 不在 cfg_tmp_prefix {cfg_tmp_prefix:?} 范围内"
                ),
            ));
        }

        // 4. head_object（IO 在 pool 上，不在 tx 里）
        let meta = cos.head_object(tmp_key).await.map_err(|e| {
            AppError::biz(
                code::BIZ_PART_FILE_TMP_OBJECT_MISSING,
                format!("head_object 失败（tmp_key={tmp_key:?}）: {e}"),
            )
        })?;
        if meta.size != file_size {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_SIZE_MISMATCH,
                format!(
                    "tmp_key={tmp_key:?} 客户端声明 size={file_size} 与服务端 head size={} 不一致",
                    meta.size
                ),
            ));
        }

        // 5. 派生 CAS key（复用 build_cas_key 模板）
        let ext = policy::ext_of(original_filename)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "缺少扩展名"))?;
        let file_type = policy::file_type_for_ext(&ext).ok_or_else(|| {
            AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("未知扩展名 {ext}"))
        })?;
        let cas_key = crate::util::cos_key::build_cas_key(
            cfg_upload_prefix,
            "part",
            owner_id,
            kind,
            sha256,
            original_filename,
        );

        // 6. copy_object（IO 在 pool 上）
        cos.copy_object(tmp_key, &cas_key).await.map_err(|e| {
            AppError::biz(
                code::BIZ_PART_FILE_UPLOAD_FAILED,
                format!("copy_object 失败（tmp={tmp_key:?} → cas={cas_key:?}）: {e}"),
            )
        })?;

        // 2026-09-16 M2-B review 第 2 轮 B2 修：copy_object 成功后**立刻** spawn
        // best-effort delete_object(tmp_key)，不等 commit。
        // - commit 成功路径：handler 后续也会 spawn 一次 delete（与此处重复），但
        //   delete_object 幂等（NoSuchKey 视为成功），不会报 21104。
        // - commit 失败路径（create_part_file 撞 23505 / tx.commit() 抛错）：此 spawn
        //   是**唯一**清理 tmp 的机会，否则 tmp 会留作孤儿（直到下次 list_objects
        //   lifecycle 兜底）。
        // 两层 spawn（service 一层 + handler 一层）双层防护；任何一层失败不阻塞另一层。
        let cos_for_early_cleanup = cos.clone();
        let tmp_key_for_early_cleanup = tmp_key.to_string();
        tokio::spawn(async move {
            if let Err(e) = cos_for_early_cleanup
                .delete_object(&tmp_key_for_early_cleanup)
                .await
            {
                tracing::warn!(
                    tmp_key = %tmp_key_for_early_cleanup,
                    error = %e,
                    "bind_uploaded_file copy 后早期 spawn delete 失败（best-effort，commit 后 handler 还会再 spawn）"
                );
            }
        });

        // 7. 开 tx：soft_delete 旧 + INSERT 新 + readback
        let mut tx = pool.begin().await?;
        // 单文件 kind（DRAWING / 3D_MODEL）下，旧活跃行要先 soft_delete
        // （`uk_t_part_file_single` 部分唯一约束）。
        let _ = sqlx::query(
            "UPDATE t_part_file \
             SET deleted_at = now(), version = version + 1, updated_at = now(), updated_by = $2 \
             WHERE part_id = $1 AND kind = $3 AND deleted_at IS NULL",
        )
        .bind(owner_id)
        .bind(current.id)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
        let new_id = snowflake.next_id();
        PartFileRepo::create_part_file(
            &mut *tx,
            NewPartFile {
                id: new_id,
                part_id: owner_id,
                owner_kind: "PART",
                kind,
                file_type,
                object_key: &cas_key,
                original_filename,
                file_size,
                content_type,
                upload_status: "READY",
                content_sha256: Some(sha256),
                created_by: current.id,
            },
        )
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(db) = &e
                && db.code().as_deref() == Some("23505")
            {
                return AppError::biz(code::BIZ_PART_FILE_DUPLICATE, "相同文件已存在");
            }
            AppError::from(e)
        })?;
        let row = PartFileRepo::get_by_id(&mut *tx, new_id, true)
            .await?
            .ok_or_else(|| AppError::internal("刚 INSERT 的 part_file 查不到"))?;
        let out = Self::render_out(&row, "PART");
        tx.commit().await?;

        Ok((out, tmp_key.to_string()))
    }
}

/// 把 `StsCredential` 转 `CosCredentialsOut`（DTO 序列化层细节）。
fn cos_credentials_out(cred: &crate::infra::sts::StsCredential) -> crate::modules::part_file::dto::CosCredentialsOut {
    crate::modules::part_file::dto::CosCredentialsOut {
        tmp_secret_id: cred.tmp_secret_id.clone(),
        tmp_secret_key: cred.tmp_secret_key.clone(),
        session_token: cred.session_token.clone(),
        expired_time: cred.expired_time,
    }
}

// ===== 2026-09-15 takeover-fill：content / delete（Phase 3 补齐） =====

/// 后端代理文件二进制流：拉 `object_key` → COS `get_object` → 透传 content_type。
///
/// 权限：4 角色任意已登录（与 `get_file_with_url` 一致）。
pub struct PartFileContent {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

impl PartFileService {
    /// `GET /api/v2/part-files/{file_id}/content`。
    pub async fn get_file_content(
        conn: &mut PgConnection,
        cos: Arc<dyn CosClient>,
        file_id: i64,
        current: &CurrentUser,
    ) -> Result<PartFileContent, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let row = PartFileRepo::get_by_id(conn, file_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_NOT_FOUND,
                    format!("part_file {file_id} 不存在"),
                )
            })?;
        let bytes = cos.get_object(&row.object_key).await?;
        Ok(PartFileContent {
            bytes,
            content_type: Some(row.content_type),
        })
    }

    /// `POST /api/v2/part-files/{file_id}/delete`。
    ///
    /// 权限：按 `kind` 派生（DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET → M+C；
    /// G_CODE → M+CNC）。
    ///
    /// 行为：乐观锁守；UPDATE `deleted_at = now()` + `version = version + 1`。
    ///
    /// 返回软删行的 `object_key`，由 **handler 在 `tx.commit()` 之后** 异步
    /// `tokio::spawn(cos.delete_object(...))`——避免 commit 失败却已触发
    /// COS 删除、孤儿对象风险（2026-09-15 review 第 1 轮 A2 修）。
    pub async fn soft_delete_file(
        conn: &mut PgConnection,
        _cos: Arc<dyn CosClient>,
        file_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<String, AppError> {
        let row = PartFileRepo::get_by_id(&mut *conn, file_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_NOT_FOUND,
                    format!("part_file {file_id} 不存在"),
                )
            })?;
        let kind = row.kind.clone();
        let object_key = row.object_key.clone();
        // 按 kind 派生权限
        match kind.as_str() {
            "DRAWING" | "3D_MODEL" | "CAD_2D" | "SETUP_SHEET" => {
                current.require_any_role(&[Role::Manager, Role::Clerk])?;
            }
            "G_CODE" => {
                current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!("kind={other:?} 不可软删（仅 DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET / G_CODE）"),
                ));
            }
        }

        let rows_affected = sqlx::query(
            "UPDATE t_part_file \
             SET deleted_at = now(), \
                 version    = version + 1, \
                 updated_at = now(), \
                 updated_by = $2 \
             WHERE id = $1 AND version = $3 AND deleted_at IS NULL",
        )
        .bind(file_id)
        .bind(current.id)
        .bind(version)
        .execute(conn)
        .await
        .map_err(AppError::from)?
        .rows_affected();
        if rows_affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part_file {file_id} 版本冲突（version={version}）"),
            ));
        }

        // 2026-09-15 review A2 修：service 不再 spawn COS delete；
        // 把 object_key 返回给 handler，由 handler commit 后再触发。
        Ok(object_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_keeps_safe_chars() {
        assert_eq!(sanitize_filename("drawing.pdf"), "drawing.pdf");
        assert_eq!(sanitize_filename("DRAW-001.PDF"), "DRAW-001.PDF");
        assert_eq!(sanitize_filename("my_drawing_v2.pdf"), "my_drawing_v2.pdf");
    }

    #[test]
    fn sanitize_filename_replaces_unsafe() {
        // 空格 / 中文 → _
        assert_eq!(sanitize_filename("图纸 v2.pdf"), "___v2.pdf");
        assert_eq!(sanitize_filename("a b/c.pdf"), "a_b_c.pdf");
    }

    #[test]
    fn sanitize_filename_truncates_long_names() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_filename(&long).len(), 80);
    }

    #[test]
    fn sanitize_filename_empty_fallback() {
        assert_eq!(sanitize_filename(""), "file");
        // 中文字符被替换为 `_`（不是 fallback）：保留断言验证替换逻辑
        assert_eq!(sanitize_filename("中文"), "__");
    }
}