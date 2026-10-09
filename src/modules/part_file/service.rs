//! part_file 域业务逻辑（2026-09-14 Phase 3 + 2026-09-16 M2-B 业务层 + 2026-09-18 upload-intents 删除 + 2026-09-22 对齐 iam 范式）
//!
//! 对应 Python myERP/service/part_file_service.py + service/_file_kind_policy.py。
//!
//! ## 上传核心逻辑（`upload_file_for_owner`）
//! 1. 取 `owner_kind`（PART / ASSEMBLY），校验存在性（21105 OWNER_NOT_FOUND）—— 走
//!    `repo.part_owner_exists` / `repo.assembly_owner_exists`
//! 2. 校验 `kind` 在白名单内（policy::allowed_exts）
//! 3. 校验扩展名（policy::ext_of）—— 无扩展名 / 不在白名单 → 21102 BAD_TYPE
//! 4. 校验 content_type（policy::expected_content_types_for_ext）—— 不匹配 → 21102
//! 5. SHA-256 算 hash → `repo.get_by_owner_kind_sha` 撞唯一索引 → 21108 DUPLICATE（CAS 去重）
//!    注意：CAS 命中时**跳过 COS PUT**，直接复用已有 object_key（节省 COS 流量）
//! 6. 上传 COS（infra/cos.rs::put_object）
//! 7. `repo.create_part_file` 写 DB
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
//! ## 2026-09-16 M2-B → 2026-09-18 STS 会话域拆分
//! - 原 `upload_intents` 业务函数（场景 A/B 一次性签 STS + CAS 去重命中复用）
//!   **2026-09-18 已删除**：迁移至相关 STS 会话域（共享 STS 凭证 + Redis 会话）。
//! - `bind_uploaded_file` 保留：confirm handler + batch_create service 共享的"已上传到
//!   tmp 区 → 绑定到 owner"逻辑；head/copy 在 tx 之外，事务内只做 soft_delete + INSERT。
//!
//! ## 2026-09-28 相关 STS 会话域下线
//! 相关 STS 会话域整体下线；前端改为单 uploader 触发时单 HTTP 调用 python `sts-tmp-keys`
//! 数组入参直签 STS 凭证。本文件保持不变（confirm 链路 + bind_uploaded_file 仍依赖
//! t_part_file + t_part_file_tmp_object 物理表，不受 STS 链路改造影响）。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范式）
//! 事务移交 handler（与 20 个 handler 文件现状对齐）：service 仅业务逻辑，所有跨
//! repo 操作经 `repo: R`（by-value；`R: PartFileRepoTrait`）参数传入——handler/service
//! 借 `&mut *tx` / `&mut *conn` 喂给 trait（trait 已直接 `impl for &mut PgConnection`）。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! `PartFileService` 字段仅 `snowflake`（事务已移交 handler）；实例为轻壳，可直接
//! `Arc<PartFileService>` 存 `AppState`；方法签名 `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`，
//! 生产 `R = &mut PgConnection`，单测 `R = MockPartFileRepoTrait` 直接注入。

use std::sync::Arc;

use sqlx::{PgConnection, PgPool};

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part_file::dto::{PartFileListQuery, validate};
use crate::modules::part_file::model::TPartFile;
use crate::modules::part_file::policy;
use crate::modules::part_file::repo::{NewPartFile, PartFileRepoTrait, hash_bytes};
use crate::modules::part_file::vo::{PartFileListOut, PartFileOut, PartFileWithUrlOut};
use crate::shared::error::{AppError, code};
use crate::util::cos_key;

pub struct PartFileService {
    snowflake: Arc<SnowflakeIdGenerator>,
    cos: Arc<dyn CosClient>,
}

impl PartFileService {
    /// 构造：雪花 ID 生成器 + COS 客户端（2026-09-22 重构：cos 从 handler 形参
    /// 收归到 service 字段，service 自管 cos 不变量；handler 仍按需
    /// `state.cos.clone()` 注入即可，service 调用 `&self.cos`）。
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>, cos: Arc<dyn CosClient>) -> Self {
        Self { snowflake, cos }
    }

    /// 单文件上传（multipart `file` 字段 + JSON `data` 字段已解析）。
    ///
    /// `original_filename` 是客户端 multipart 的 filename；`content_type` 是
    /// 客户端声明的 MIME；`bytes` 是文件二进制。
    ///
    /// 返回新建的 `PartFileOut`（含 id）。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file_for_owner<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
        original_filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
        current: &CurrentUser,
    ) -> Result<PartFileOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::CncProgrammer])?;

        // 1. owner 存在性校验（走 trait helper）
        Self::assert_owner_exists(&mut repo, owner_kind, owner_id).await?;

        // 2. kind 白名单（policy::allowed_exts 自动挡掉未知 kind）
        let ext = policy::ext_of(original_filename).ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("文件缺少扩展名: {original_filename:?}"),
            )
        })?;
        let allowed_exts = policy::allowed_exts(kind);
        if !allowed_exts.contains(&ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("kind={kind:?} 不接受扩展名 {ext:?}"),
            ));
        }
        // 3. content_type 校验
        let expected = policy::expected_content_types_for_ext(&ext);
        if !expected
            .iter()
            .any(|c| c.eq_ignore_ascii_case(content_type))
        {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!(
                    "kind={kind:?}, 扩展名={ext:?}: content_type {content_type:?} 不在白名单 {expected:?}"
                ),
            ));
        }
        // 4. file_type 推导
        let file_type = policy::file_type_for_ext(&ext).ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("扩展名 {ext:?} 无对应 file_type"),
            )
        })?;

        // 5. SHA-256 → CAS 去重（撞唯一索引 → 21108 DUPLICATE）
        let sha = hash_bytes(&bytes);
        if let Some(existing) = repo.get_by_owner_kind_sha(owner_id, kind, &sha).await? {
            // CAS 命中：跳过 COS PUT，直接复用已有记录（返回 id / object_key）
            return Ok(Self::render_out(&existing, owner_kind));
        }

        // 6. 上传 COS（2026-09-29 扁平化：key 模板五段→两段 `{prefix}{sha16}_{safe_filename}`，
        //    owner_kind/owner_id/KIND 信息已在 t_part_file DB 行外键索引，不再写进 key）。
        let object_key = cos_key::build_cas_key("", &sha, original_filename);
        self.cos
            .put_object(&object_key, bytes.clone(), content_type)
            .await?;

        // 7. INSERT t_part_file
        let nf = NewPartFile {
            id: self.snowflake.next_id(),
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
        let id = repo.create_part_file(nf).await.map_err(|e| match e {
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
        let row = repo
            .get_by_id(id, true)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_NOT_FOUND, "刚 INSERT 却查不到"))?;
        Ok(Self::render_out(&row, owner_kind))
    }

    /// 列表：按 owner_kind + owner_id + kind 过滤 + 分页。
    ///
    /// 权限（2026-10-10 新增 `Role::ShelfAccount`）：5 角色。放开给工控机账号是因为
    /// 报工台（`/scan/*`，路由守卫就是 `SHELF_ACCOUNT`）的图纸预览要拉这份列表。
    /// ⚠️ **只读端点**：上传 `upload_file_for_owner` 与软删 `soft_delete_file` 的角色
    /// 白名单**不含** `ShelfAccount`，两者是写路径、不在本次放开范围。
    pub async fn list_files<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        query: &PartFileListQuery,
        current: &CurrentUser,
    ) -> Result<PartFileListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
            Role::ShelfAccount,
        ])?;
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

        let (rows, total) = repo
            .list_with_filters(owner_kind, owner_id_opt, kind_opt, limit, offset)
            .await?;
        let items = rows
            .into_iter()
            .map(|r| Self::render_out(&r, owner_kind))
            .collect();
        Ok(PartFileListOut { items, total })
    }

    /// 单条详情 + 预签下载 URL。
    ///
    /// 2026-09-29 扁平化：DB 历史行 object_key 仍是五段（legacy），但 COS 桶内
    /// 真实对象可能按新模板存储（同一文件内容 sha16 一致 → 新模板 key 一致）。
    /// 读端点 fallback：先尝试 `head_object(db_key)`，若 NoSuch 且 db_key 可被
    /// `parse_legacy_key` 解析，则按新模板重写 key 再 head 一次；若新模板 key
    /// 存在则用其生成 presigned URL（保证浏览器拿到的 URL 在桶内有效）。
    ///
    /// ⚠️ **白名单刻意不含 `Role::ShelfAccount`**（2026-10-10）：本端点回的是 **COS
    /// 预签直链**（有效期 1 小时），拿到即可绕过本后端任意访问、直链可外传。报工台
    /// 预览用的是同域的 `get_file_content`（走后端代理、同样能看内容），不需要预签。
    /// 要放开「工控机看图纸」时放开 content 即可，不要顺带放开这里。
    #[allow(clippy::too_many_arguments)]
    pub async fn get_file_with_url<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        cos: Arc<dyn CosClient>,
        cfg_upload_prefix: &str,
        file_id: i64,
        current: &CurrentUser,
    ) -> Result<PartFileWithUrlOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let row = repo.get_by_id(file_id, false).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_NOT_FOUND,
                format!("part_file {file_id} 不存在"),
            )
        })?;
        let effective_key =
            Self::resolve_effective_key(cos.as_ref(), cfg_upload_prefix, &row.object_key).await?;
        let url = cos.presigned_get_url(&effective_key, 3600).await?;
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

    /// 2026-09-29 新增：解析 `db_object_key` 的「有效 COS key」。
    ///
    /// 逻辑：
    /// 1. 先 `head_object(db_key)` —— 存在 → 直接返回 `db_key`
    /// 2. 不存在 + `parse_legacy_key(db_key)` 成功 → 重写成新模板 `new_key`，
    ///    `head_object(new_key)` —— 存在则返回 `new_key`
    /// 3. 都不存在 → 返回 `db_key`（调用方后续 get_object 会拿到 NoSuch，由 handler 转 21114）
    ///
    /// 设计取舍：head 调用至多 2 次（db_key + 一次 legacy rewrite）；生产冷数据
    /// 95%+ 应在第 1 次命中。完全 fallback 失败时返回 db_key（保留原行为），不让
    /// 迁移 bin 之外的因素阻断生产读路径。
    pub async fn resolve_effective_key(
        cos: &dyn CosClient,
        cfg_upload_prefix: &str,
        db_object_key: &str,
    ) -> Result<String, AppError> {
        if cos.head_object(db_object_key).await.is_ok() {
            return Ok(db_object_key.to_string());
        }
        if let Some(lk) = cos_key::parse_legacy_key(db_object_key) {
            let new_key = cos_key::rewrite_legacy_to_new(cfg_upload_prefix, &lk);
            if cos.head_object(&new_key).await.is_ok() {
                tracing::info!(
                    db_key = %db_object_key,
                    new_key = %new_key,
                    "part_file 读端点 legacy → 新模板 fallback 命中"
                );
                return Ok(new_key);
            }
        }
        Ok(db_object_key.to_string())
    }

    /// 校验 owner 存在性（polymorphic，走 trait helper）。
    async fn assert_owner_exists<R: PartFileRepoTrait>(
        repo: &mut R,
        owner_kind: &str,
        owner_id: i64,
    ) -> Result<(), AppError> {
        match owner_kind {
            "PART" => {
                if !repo.part_owner_exists(owner_id).await? {
                    return Err(AppError::biz(
                        code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                        format!("part {owner_id} 不存在"),
                    ));
                }
            }
            "ASSEMBLY" => {
                if !repo.assembly_owner_exists(owner_id).await? {
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

// 2026-09-29 扁平化：旧的本地 `sanitize_filename` 函数 + 其单元测试已删除，
// 统一走 `util::cos_key::safe_filename` 单一真相源。

/// 公开给装配体 upload-files 端点用的辅助：列出某 owner 的 part_file（不限定 kind）。
#[allow(dead_code)]
pub async fn list_part_files_for_owner(
    conn: &mut PgConnection,
    owner_kind: &str,
    owner_id: i64,
) -> Result<Vec<TPartFile>, sqlx::Error> {
    crate::modules::part_file::repo::PartFileRepo::list_by_owner(conn, owner_kind, owner_id).await
}

// ===== 2026-09-16 M2-B 业务层：bind_uploaded_file（confirm + batch_create 共享） ==============
//
// 2026-09-18 注：原 `upload_intents` 业务函数已**删除**（迁移至相关 STS 会话域）。
// 2026-09-28 备注：相关 STS 会话域整体下线；该函数不再有任何归属。
// 保留 `bind_uploaded_file` 给 confirm handler + batch_create service 共用：
// head/copy 在 tx 之外，事务内只做 soft_delete + INSERT。
//
// 2026-09-22 重构说明：本方法是「事务由 service 自管」的特例——
// - 调用方：`part/handler/batch.rs::confirm_part_file`（part 域，不在本 worktree 范围）
//   + `part/service/batch.rs::prepare_binding_head_copy`（part 域，同样不在范围）。
// - 两者当前都是 `PartFileService::bind_uploaded_file(...)` 静态调用风格（无 `&self`）。
// - 为避免改动 part 域（spec §禁区 #7：禁止碰其他 13 域），本方法保持原签名
//   （pool / snowflake / cos 全部参数化），由 service 自行 `pool.begin()` 开 tx；
//   tx 内 DB 操作仍走 trait 模式（`&mut *tx` 直接喂给 `PartFileRepoTrait`）。
// - 这是 handler 移交事务范式的**唯一例外**，余下 5 个 part_file 端点
//   （upload/list/get_url/get_content/soft_delete）均按 handler 管 tx 范式改造。

impl PartFileService {
    /// 共享 service：把已上传到 COS tmp 区的一个对象绑定到 owner（INSERT t_part_file）。
    ///
    /// 调用方：`confirm handler`（T2.6）+ `batch_create service`（T2.7）。
    ///
    /// 流程：
    /// 1. `tmp_key` 前缀防呆：必须以 `cfg_tmp_prefix` 开头（防客户端乱传 key 读到别人文件）
    /// 2. `head_object` 校验对象存在 + size 与声明一致：
    ///    - 不存在 → `BIZ_PART_FILE_TMP_OBJECT_MISSING` 21114
    ///    - size 不一致 → `BIZ_PART_FILE_SIZE_MISMATCH` 21115
    /// 3. 派生 CAS key：`util::cos_key::build_cas_key(prefix, "part", owner_id, kind, sha16, filename)`
    /// 4. `cos.copy_object(tmp_key, cas_key)` —— 走 PermanentCos（不走 STS）
    /// 5. copy_object 成功后**立刻 spawn** best-effort delete_object(tmp_key) 早期清理
    /// 6. 开 tx：soft_delete 旧活跃行（`uk_t_part_file_single` 部分唯一约束） + INSERT 新行
    /// 7. commit
    /// 8. 返回 `(PartFileOut, tmp_key)` —— caller 拿到 tmp_key 后**已 commit**，再
    ///    spawn 异步 delete_object(tmp_key) 兜底清理
    ///
    /// **重要**：head/copy 是外部 IO，**不**放进事务 tx 里——tx 里只做 DB（事务回滚时
    /// IO 已发生难恢复），依赖 5 / 8 两层 spawn 双层防护。
    ///
    /// 注：pool 直接传（不是 `&mut tx`）—— head/copy 用 pool 直连，确保与 tx 隔离。
    /// 本方法自管 tx（part 域 caller 不开 tx），不依赖 handler 借连接——是事务范式的
    /// 唯一例外（详见模块 doc-comment）。
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
                format!("tmp_key {tmp_key:?} 不在 cfg_tmp_prefix {cfg_tmp_prefix:?} 范围内"),
            ));
        }

        // 4. head_object（IO，无 DB）
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

        // 5. 派生 CAS key（2026-09-29 扁平化：模板五段→两段）
        let ext = policy::ext_of(original_filename)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "缺少扩展名"))?;
        let file_type = policy::file_type_for_ext(&ext).ok_or_else(|| {
            AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("未知扩展名 {ext}"))
        })?;
        let cas_key = cos_key::build_cas_key(cfg_upload_prefix, sha256, original_filename);

        // 6. copy_object（IO，无 DB）
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

        // 7. 开 tx：soft_delete 旧 + INSERT 新 + readback（事务范式特例：本方法自管 tx）
        let mut tx = pool.begin().await?;
        // 单文件 kind（DRAWING / 3D_MODEL）下，旧活跃行要先 soft_delete
        // （`uk_t_part_file_single` 部分唯一约束）。
        let mut repo: &mut sqlx::PgConnection = &mut tx;
        let _ = PartFileRepoTrait::soft_delete_active_by_part_kind(
            &mut repo, owner_id, current.id, kind,
        )
        .await?;
        let new_id = snowflake.next_id();
        PartFileRepoTrait::create_part_file(
            &mut repo,
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
        let row = PartFileRepoTrait::get_by_id(&mut repo, new_id, true)
            .await?
            .ok_or_else(|| AppError::internal("刚 INSERT 的 part_file 查不到"))?;
        let out = Self::render_out(&row, "PART");
        tx.commit().await?;

        Ok((out, tmp_key.to_string()))
    }
}

// ===== 2026-09-15 takeover-fill：content / delete（Phase 3 补齐） ==========================

/// 后端代理文件二进制流：拉 `object_key` → COS `get_object` → 透传 content_type。
///
/// 权限：`get_file_content` 的 5 角色白名单
/// （Manager / Clerk / Inspector / CncProgrammer / **ShelfAccount**，2026-10-10 新增
/// ShelfAccount —— 报工台工控机账号预览图纸）。⚠️ 与 `get_file_with_url` **同角色集合
/// 但不同风险面**：本端点由后端代理、经 RBAC 逐次把关；`/url` 回的是 COS 预签直链，
/// 拿到即可脱离本后端直接访问。
pub struct PartFileContent {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

impl PartFileService {
    /// `GET /api/v2/part-files/{file_id}/content`。
    ///
    /// 2026-09-29 扁平化：与 `get_file_with_url` 同款 legacy→新模板 fallback，
    /// 详见 `resolve_effective_key`。
    ///
    /// 权限（2026-10-10 新增 `Role::ShelfAccount`）：与 `list_files` 同一套 5 角色。
    /// 报工台（`/scan/*`）跑的就是 `SHELF_ACCOUNT` 账号，其图纸预览打的是本端点
    /// （后端代理二进制流）—— 不放开则预览必然 403（业务码 40300）。
    pub async fn get_file_content<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        cos: Arc<dyn CosClient>,
        cfg_upload_prefix: &str,
        file_id: i64,
        current: &CurrentUser,
    ) -> Result<PartFileContent, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
            Role::ShelfAccount,
        ])?;
        let row = repo.get_by_id(file_id, false).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_NOT_FOUND,
                format!("part_file {file_id} 不存在"),
            )
        })?;
        let effective_key =
            Self::resolve_effective_key(cos.as_ref(), cfg_upload_prefix, &row.object_key).await?;
        let bytes = cos.get_object(&effective_key).await?;
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
    pub async fn soft_delete_file<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        file_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<String, AppError> {
        let row = repo.get_by_id(file_id, false).await?.ok_or_else(|| {
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
                    format!(
                        "kind={other:?} 不可软删（仅 DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET / G_CODE）"
                    ),
                ));
            }
        }

        let rows_affected = repo
            .soft_delete_file(file_id, version, current.id)
            .await
            .map_err(AppError::from)?;
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
    // 2026-09-29 扁平化：sanitize_filename 已迁至 util::cos_key::safe_filename，
    // 其单元测试也迁至 util::cos_key::tests。本 mod 暂时无 unit test 残留。
}
