//! part_file 域业务逻辑（2026-09-14 Phase 3 + 2026-09-16 M2-B 业务层 + 2026-09-18 upload-intents 删除 + 2026-09-22 对齐 iam 范式）
//!
//! 对应 Python myERP/service/part_file_service.py + service/_file_kind_policy.py。
//!
//! ## 上传链路（2026-10-15 统一：预签 PUT 直传）
//!
//! ```text
//! ① POST /files/objects/upload-intents  → grant_upload_intents（纯读 + 一次签名）
//! ② 前端 XHR PUT 直传 COS（字节不过本进程）
//! ③ POST /files/objects/confirm       → bind_uploaded_file（head 校验 + INSERT）
//! ```
//!
//! ①②③ 之外本域**没有任何**收字节的入口：`POST /files/objects`（multipart 代理）
//! 与 `POST /api/v2/files/sts-tmp-keys`（python STS 转发）均已下线。
//!
//! 两条写路径共用的校验原语全在 `part_file::policy` 与 `dto::validate`：
//! `owner_kind` × `kind` 配对、扩展名白名单、content_type 白名单、写路径角色分派。
//!
//! ## 与 part / assembly 域的耦合
//! **恒为「零」**：`t_part_file` 的写入路径只有本子模块那两条端点。
//! 装配体 PDF（`kind='ASSEMBLY_MASTER'`）走同一条通道，`owner_kind='ASSEMBLY'`；
//! 客户送货单打印模板（`kind='DELIVERY_TEMPLATE'`）走 `owner_kind='CUSTOMER'`。
//!
//! ## 2026-10-13 PR1b：COS key 前缀统一 `obj/`
//! 上传写入的 CAS key 前缀取自 `cfg.cos.upload_prefix`（缺省 `obj/`），读端点的
//! legacy fallback 用**同一份**配置。⚠️ **不需要跑 `cos_key_migrate` 做数据迁移**：
//! `t_part_file.object_key` 存的是完整 key，`resolve_effective_key` 先
//! `head_object(db_object_key)`、命中就直接返回 ⇒ 历史上落在 `drawings/` 或桶根的
//! 对象原地不动，本轮只是让**新上传**统一落 `obj/`。
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

use sqlx::PgPool;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::files::part_file::dto::{PartFileListQuery, UploadIntentItemIn, validate};
use crate::modules::files::part_file::model::TPartFile;
use crate::modules::files::part_file::policy;
use crate::modules::files::part_file::repo::{NewPartFile, PartFileRepoTrait};
use crate::modules::files::part_file::vo::{
    PartFileListOut, PartFileOut, PartFileWithUrlOut, UploadIntentItemOut, UploadIntentsOut,
};
use crate::shared::error::{AppError, code};
use crate::util::cos_key;

/// `POST /files/objects/upload-intents` 单请求最多签发多少个文件。
///
/// 2026-10-15：与 `part::service::BATCH_CREATE_PARTS_MAX_ITEMS`（200）取同一个量级
/// —— 一次多选几百个文件的那是「建单」场景，不是「传文件」场景。
pub const UPLOAD_INTENTS_MAX_ITEMS: usize = 200;

pub struct PartFileService {
    snowflake: Arc<SnowflakeIdGenerator>,
    cos: Arc<dyn CosClient>,
}

/// 本域通用列表端点（`GET /files/objects`）**永不返回**的 kind。
///
/// 2026-10-16 review 第 1 轮 MAJOR-3：送货单打印模板行的元数据（含一级客户 id）
/// 只应经 `print_template` 域的端点暴露，而那个端点集是 Manager-only（列表）
/// 或权限点 `delivery_note.print`（resolve / content）。⚠️ 这里的守卫是**无条件**
/// 的 —— 即便调用方显式传 `?kind=DELIVERY_TEMPLATE` 也不返回（理由见
/// `list_files` 的 doc）。
const TEMPLATE_KIND_EXCLUDED_FROM_LIST: &str = "DELIVERY_TEMPLATE";

impl PartFileService {
    /// 构造：雪花 ID 生成器 + COS 客户端（2026-09-22 重构：cos 从 handler 形参
    /// 收归到 service 字段，service 自管 cos 不变量；handler 仍按需
    /// `state.cos.clone()` 注入即可，service 调用 `&self.cos`）。
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>, cos: Arc<dyn CosClient>) -> Self {
        Self { snowflake, cos }
    }

    /// `POST /api/v2/files/objects/upload-intents`：按内容 sha 派生**正式 CAS key**，
    /// 签发预签 PUT URL（前端直传 COS）。
    ///
    /// 2026-10-15 新增。**只读**（SELECT + 一次 cos 签名，无字节、无 INSERT）——
    /// 登记发生在随后的 `POST /files/objects/confirm`。
    ///
    /// ## 校验顺序（先命中先返回，错误码即优先级）
    ///
    /// | 步 | 校验 | 码 |
    /// |---|---|---|
    /// | 1 | `items` 非空且 ≤ 200 | 40001 |
    /// | 2 | 角色闸门（`policy::write_roles_for`，口径与 multipart 上传逐字一致） | 40300 |
    /// | 3 | `owner_kind` × `kind` 配对（逐项） | 20104 |
    /// | 4 | owner 存在性（CUSTOMER 仍要求一级客户） | 21105 |
    /// | 5 | 逐项：sha 64hex / filename ≤255 / content_type 非空 | 40001 |
    /// | 6 | 逐项：扩展名在 `policy::allowed_exts(kind)` 内 + content_type 匹配 | 21102 |
    /// | 7 | 逐项：`0 < file_size ≤ cos.max_file_size` | 40001 / 21103 |
    /// | 8 | 逐项：CAS 命中活跃行 ⇒ `already_exists`，**不签 URL** | — |
    /// | 9 | 逐项：`build_cas_key` + `cos.presigned_put_url` | — |
    ///
    /// ## 为什么命中 CAS 就**不**签 URL
    ///
    /// 那样前端拿到的就是一条「写这个既有 key」的授权 —— 而那个 key 已经被一条活跃
    /// 行引用着，前端却不需要真的写它。秒传的口号是「一次 PUT 都不发」。
    #[allow(clippy::too_many_arguments)]
    pub async fn grant_upload_intents<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        cfg_upload_prefix: &str,
        cfg_max_file_size: usize,
        cfg_url_expire_seconds: u32,
        owner_kind: &str,
        owner_id: i64,
        items: &[UploadIntentItemIn],
        current: &CurrentUser,
    ) -> Result<UploadIntentsOut, AppError> {
        // 1. 批量形状校验（与 `PartBatchCreateRequest` 同款「先形状后内容」）
        if items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        if items.len() > UPLOAD_INTENTS_MAX_ITEMS {
            return Err(AppError::validation(format!(
                "items 数量 {} 超过上限 {UPLOAD_INTENTS_MAX_ITEMS}",
                items.len()
            )));
        }

        // 2. 角色闸门：与 `upload_file_for_owner`（multipart 上传）**逐字同口径**
        //    —— PART/ASSEMBLY → M+C+CNC，CUSTOMER（送货单打印模板）→ 仅 Manager。
        //    两条写入口必须共用 `policy::write_roles_for`，否则「谁能写
        //    DELIVERY_TEMPLATE」的收口点会分叉（见该函数的 doc）。
        current.require_any_role(policy::write_roles_for(
            owner_kind,
            policy::WriteEntry::Upload,
        ))?;

        // 3. owner_kind × kind 配对（纯内存，逐项；排在 DB 往返之前）
        for it in items {
            Self::assert_owner_kind_matches_kind(owner_kind, &it.kind)?;
        }

        // 4. owner 存在性（一次往返；CUSTOMER 臂含 `parent_id IS NULL` 判据）
        Self::assert_owner_exists(&mut repo, owner_kind, owner_id).await?;

        let mut out_items = Vec::with_capacity(items.len());
        for it in items {
            // 5. 逐项字段形状校验（与 confirm 共用同一批 helper，报 40001）
            validate::check_sha256(&it.content_sha256)?;
            validate::check_filename(&it.original_filename)?;
            if it.content_type.trim().is_empty() {
                return Err(AppError::validation(format!(
                    "client_ref={} 的 content_type 不可为空",
                    it.client_ref
                )));
            }

            // 6. 扩展名白名单 + content_type 匹配（21102：请求指向的类型不对）
            let ext = policy::ext_of(&it.original_filename).ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!("文件缺少扩展名: {:?}", it.original_filename),
                )
            })?;
            if !policy::allowed_exts(&it.kind).contains(&ext.as_str()) {
                return Err(AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!("kind={:?} 不接受扩展名 {ext:?}", it.kind),
                ));
            }
            let expected = policy::expected_content_types_for_ext(&ext);
            if !expected
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&it.content_type))
            {
                return Err(AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!(
                        "kind={:?}, 扩展名={ext:?}: content_type {:?} 不在白名单 {expected:?}",
                        it.kind, it.content_type
                    ),
                ));
            }

            // 7. 大小：0 → 40001（无效输入），超限 → 21103（与 multipart 上传同码）
            validate::check_file_size(it.file_size, cfg_max_file_size)?;

            // 8. CAS 查重：命中**活跃行**即秒传，不签 URL
            if let Some(existing) = repo
                .get_by_owner_kind_sha(owner_id, &it.kind, &it.content_sha256)
                .await?
            {
                out_items.push(UploadIntentItemOut {
                    client_ref: it.client_ref.clone(),
                    already_exists: true,
                    object_key: None,
                    upload_url: None,
                    content_type: None,
                    expires_in: cfg_url_expire_seconds,
                    existing_file: Some(Self::render_out(&existing, owner_kind)),
                });
                continue;
            }

            // 9. 派生正式 CAS key + 签预签 PUT URL（前端拿它直传，字节不过 rust）
            let object_key = cos_key::build_cas_key(
                cfg_upload_prefix,
                &it.content_sha256,
                &it.original_filename,
            );
            let signed = self
                .cos
                .presigned_put_url(&object_key, &it.content_type, cfg_url_expire_seconds)
                .await?;
            out_items.push(UploadIntentItemOut {
                client_ref: it.client_ref.clone(),
                already_exists: false,
                object_key: Some(object_key),
                upload_url: Some(signed.url),
                content_type: Some(signed.content_type),
                expires_in: cfg_url_expire_seconds,
                existing_file: None,
            });
        }

        Ok(UploadIntentsOut { items: out_items })
    }

    /// 列表：按 owner_kind + owner_id + kind 过滤 + 分页。
    ///
    /// 权限（2026-10-10 新增 `Role::ShelfAccount`）：5 角色。放开给工控机账号是因为
    /// 报工台（`/scan/*`，路由守卫就是 `SHELF_ACCOUNT`）的图纸预览要拉这份列表。
    /// ⚠️ **只读端点**：上传 `upload_file_for_owner` 与软删 `soft_delete_file` 的角色
    /// 白名单**不含** `ShelfAccount`，两者是写路径、不在本次放开范围。
    ///
    /// ⚠️ **2026-10-16 review 第 1 轮 MAJOR-3：本端点永不返回 `DELIVERY_TEMPLATE`
    /// 行**（`?kind=DELIVERY_TEMPLATE` 也一样返空，不是「缺省时才排除」）。
    /// 理由：出参含 `owner_id`(= 一级客户 id) / `object_key` / `original_filename` /
    /// `content_sha256`，而本端点的角色集合**含 `SHELF_ACCOUNT`**（工控机）——
    /// 不加这道过滤，工控机账号不传 `kind` 就能列出全部客户的打印模板元数据并拿到
    /// 模板 id，那正是「按 id 取字节」越权链的「发现」环节。
    /// ⚠️ **不传 `kind` 也排除**：只在缺省时排除的话，攻击者补一个
    /// `?kind=DELIVERY_TEMPLATE` 就绕回来了（`kind` 是自由文本，不做枚举校验）。
    /// 模板行要列举只能打 `print_template` 域的 `GET /print-templates`
    /// （**Manager-only**，管理面），单客户定向取打 `GET /print-templates/resolve`
    /// （权限点 `delivery_note.print`）—— 两者的权限面都比本端点窄或语义不同。
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
            .list_with_filters(
                owner_kind,
                owner_id_opt,
                kind_opt,
                Some(TEMPLATE_KIND_EXCLUDED_FROM_LIST),
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

    /// 取单条 `t_part_file` 行（软删闸门），供**域内其它只读子模块**复用。
    ///
    /// 2026-10-13 PR2 新增，为 `files::part_file::preview` 而加：预览的 seed 推导
    /// 需要 `content_sha256` / `object_key` / `kind` / `file_type` 四个字段，而
    /// 现有读端点返回的都是**出参 VO**（`PartFileOut` / `PartFileWithUrlOut`），
    /// 形态对不上。
    ///
    /// ⚠️ **刻意不收 `CurrentUser`、也不做角色判定**：本方法只是「取一行」，权限由
    /// 调用方（预览 service）自己的 `require_any_role` 承担。在这里判会把预览的
    /// 角色矩阵（5 角色，含与 `/url` 相反的 `ShelfAccount`）和 part_file 的其它
    /// 端点搅在一起。
    pub async fn get_file_row<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        file_id: i64,
    ) -> Result<TPartFile, AppError> {
        repo.get_by_id(file_id, false).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_NOT_FOUND,
                format!("part_file {file_id} 不存在"),
            )
        })
    }

    /// 单条详情 + 预签下载 URL。
    ///
    /// 权限：**权限点 `part.drawing.print`**（2026-10-14 由 4 角色白名单迁移；
    /// 与 `part` 域图纸打印端点共用同一点）。默认矩阵逐格等于迁移前集合
    /// （MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER，见 `seeds/rbac.sql`），
    /// 但矩阵归管理员管：授权后放行、收回后拒绝都发生在面板改矩阵之后。
    /// **SHELF_ACCOUNT 默认不在矩阵内** —— 报工台工控机预览走同域的
    /// `GET /objects/{id}/content`（后端代理，见
    /// [`Self::get_file_content_of_checked_row`]）即可。
    ///
    /// ⚠️ 本端点回的是 **COS 预签直链**（有效期由 `COS_PRESIGN_EXPIRE` 下发，
    /// 代码缺省 3600s），拿到即可绕过本后端任意访问、直链可外传。content 不同：
    /// 字节流由后端代理、每次访问都重新过 RBAC。要放开「工控机看图纸」时放开
    /// content 即可，不要顺带把这里的授权也发出去。
    ///
    /// 2026-09-29 扁平化：DB 历史行 object_key 仍是五段（legacy），但 COS 桶内
    /// 真实对象可能按新模板存储（同一文件内容 sha16 一致 → 新模板 key 一致）。
    /// 读端点 fallback：先尝试 `head_object(db_object_key)`，若 NoSuch 且 db_key 可被
    /// `parse_legacy_key` 解析，则按新模板重写 key 再 head 一次；若新模板 key
    /// 存在则用其生成 presigned URL（保证浏览器拿到的 URL 在桶内有效）。
    ///
    /// ⚠️ **2026-10-16 review 第 1 轮 MAJOR-2：kind 白名单（`policy::
    /// is_file_content_kind`）**。本方法与 `GET /objects/{id}/content` 是同一个
    /// 「按 id 拿字节」动作，而先前只有后者判 kind ⇒ CLERK / INSPECTOR /
    /// CNC_PROGRAMMER（权限点 `part.drawing.print` 的默认矩阵）能拿到**任意客户**
    /// 送货单打印模板的 1 小时直链，字节直接可下载、完全绕过 content 侧那道守卫。
    /// 报错码 21102（与 content 侧同款，理由见 `policy::is_file_content_kind` 的 doc）。
    /// 判定排在 `head_object` **之前** ⇒ 非法 kind 零外部 IO。
    #[allow(clippy::too_many_arguments)]
    pub async fn get_file_with_url<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        cos: Arc<dyn CosClient>,
        cfg_upload_prefix: &str,
        cfg_presign_expire_seconds: u32,
        file_id: i64,
        current: &CurrentUser,
    ) -> Result<PartFileWithUrlOut, AppError> {
        current.require_permission("part.drawing.print")?;
        let row = repo.get_by_id(file_id, false).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_NOT_FOUND,
                format!("part_file {file_id} 不存在"),
            )
        })?;
        if !policy::is_file_content_kind(&row.kind) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!(
                    "kind={:?} 不走本端点取预签直链（仅 DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D）",
                    row.kind
                ),
            ));
        }
        let effective_key =
            Self::resolve_effective_key(cos.as_ref(), cfg_upload_prefix, &row.object_key).await?;
        let url = cos
            .presigned_get_url(&effective_key, cfg_presign_expire_seconds)
            .await?;
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
            url_expires_in_seconds: cfg_presign_expire_seconds,
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

    /// `owner_kind` × `kind` 配对守卫（2026-10-13 review 第 1 轮 MINOR-3）。
    ///
    /// 允许对见 [`policy::owner_kind_allows_kind`]（表与理由都在那里）。
    /// 错误码用 `BIZ_INVALID_VALUE`（20104），与「`owner_kind` 不在
    /// `{PART, ASSEMBLY}`」那道守卫同款 —— 两者都是「请求里这对字段的组合非法」，
    /// 而不是文件本身有问题（后者才是 21102 `BIZ_PART_FILE_BAD_TYPE`）。
    ///
    /// 两条写路径（`upload_file_for_owner` multipart 上传 / `bind_uploaded_file`
    /// STS 直传回执）都调它：`policy::allowed_exts` 新增 `ASSEMBLY_MASTER` 顺带把
    /// confirm 路径的接受面一起放大了（`dto::validate::check_kind` 复用同一张
    /// 白名单），不放这里的话 confirm 能造出同样的「写得了、删不了」行。
    fn assert_owner_kind_matches_kind(owner_kind: &str, kind: &str) -> Result<(), AppError> {
        if !policy::owner_kind_allows_kind(owner_kind, kind) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                // ⚠️ 2026-10-16 review 第 2 轮 NIT-A：这张枚举表原先只列
                // PART / ASSEMBLY，漏了 `CUSTOMER => DELIVERY_TEMPLATE`
                // （2026-10-16 加的第三种 owner）⇒ 报错文案会把人往错方向带。
                format!(
                    "owner_kind={owner_kind:?} 与 kind={kind:?} 不是允许的组合（PART 收 DRAWING / 3D_MODEL，ASSEMBLY 收 ASSEMBLY_MASTER，CUSTOMER 收 DELIVERY_TEMPLATE）"
                ),
            ));
        }
        Ok(())
    }

    /// 校验 owner 存在性（polymorphic，走 trait helper）。
    ///
    /// 2026-10-16 新增 `"CUSTOMER"` 臂（送货单打印模板挂在**一级客户**上）。
    /// 错误码沿用 `BIZ_PART_FILE_OWNER_NOT_FOUND`（21105），与另两种 owner 同款。
    ///
    /// ⚠️ **2026-10-16 review 第 2 轮 MAJOR-A：CUSTOMER 臂现在也判「是不是一级客户」**
    /// （谓词在 `repo::customer_owner_exists`，本轮给它补了 `AND parent_id IS NULL`）。
    /// 首版这里**只判「存在且未软删」**、把 `parent_id IS NULL` 当成 `print_template`
    /// 域独有的强制点 —— 于是 `POST /files/objects`（本方法的调用方之一，`owner_kind`
    /// / `owner_id` 全由 client 在 multipart `data` 里给）能给**二级客户**落一行模板：
    /// ① 破坏「只绑一级客户」不变式；② `uk_t_part_file_single` 是 `(part_id, kind)`，
    /// L1 行与 L2 行互不冲突 ⇒「一个 L1 至多一份活跃模板」被绕过，且给 L1 重传时
    /// `soft_delete_active_by_part_kind(l1_id)` 清不掉 L2 那行；③ 该行 `resolve`
    /// 永远读不到（送货单的 `customer_id` 是 L1）⇒ 永不消费、永不清理的孤儿配置行。
    ///
    /// ⇒ **两道闸门现在共有这条判据**：本方法（files 域兜底、21105、排在 COS PUT
    /// 之前）与 `print_template::service::l1_customer_name`（该域自己的前置强制点、
    /// 20104、零外部 IO）。正常路径永远先被后者拦下（错误码 20104），本方法管的是
    /// 「有人绕过那个域的 handler 直调共用上传原语」这条路径。**不要**因为
    /// 「本域不该知道客户分两级」把谓词去掉 —— 本域判的是 `parent_id IS NULL` 这个
    /// **列取值**，与它判 `t_part.assembly_id IS NULL`（散件）同形。
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
            "CUSTOMER" => {
                // ⚠️ 2026-10-16 review 第 2 轮 MAJOR-A：谓词含 `parent_id IS NULL`
                // （一级客户），首版只判存在 ⇒ Manager 经 `POST /files/objects`
                // 能把模板绑到二级客户上。理由见本方法的 doc。
                if !repo.customer_owner_exists(owner_id).await? {
                    return Err(AppError::biz(
                        code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                        format!(
                            "customer {owner_id} 不存在 / 已软删 / 不是一级客户（送货单打印模板只绑一级客户，parent_id IS NULL）"
                        ),
                    ));
                }
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("owner_kind {other:?} 不支持（仅 PART / ASSEMBLY / CUSTOMER）"),
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

// 2026-10-13：`list_part_files_for_owner`（原 `#[allow(dead_code)]`，本就是 0 调用方）
// 已删除。要列某 owner 的 part_file 直接走 `PartFileRepo::list_by_owner(conn, kind, id)`。

// ===== 2026-09-16 M2-B 业务层：bind_uploaded_file（STS 直传回执） ==============
//
// 2026-09-18 注：原 `upload_intents` 业务函数已**删除**（迁移至相关 STS 会话域）。
// 2026-09-28 备注：相关 STS 会话域整体下线；该函数不再有任何归属。
// 2026-10-13 PR1b：调用方只剩本子模块的 confirm 端点 handler（原 part 域
// `part/handler/batch.rs::confirm_part_file` 随端点搬来）。part 域
// `service/batch.rs::prepare_binding_head_copy` **不**调本方法 —— 它自带一份
// head/copy 预处理（DB 写入由 batch_create 第二遍在事务内做），只共用
// `util::cos_key::build_cas_key` 与 `dto::validate` 两处 helper。
//
// 2026-09-22 重构说明：本方法是「事务由 service 自管」的特例——
// - 调用方是 confirm handler，它不开 tx（head/copy 是外部 IO，不该包在事务里）。
// - 本方法保持原签名（pool / snowflake / cos 全部参数化），由 service 自行
//   `pool.begin()` 开 tx；tx 内 DB 操作仍走 trait 模式（`&mut *tx` 直接喂给
//   `PartFileRepoTrait`）。
// - 这是 handler 移交事务范式的**唯一例外**，余下 **5** 个 part_file 端点
//   （upload / list / get_url / get_content / soft_delete）均按 handler 管 tx 范式改造。
//   （2026-10-13 review 第 1 轮 NIT-10 订正：原文写「6 个」并把 `soft_delete` 与
//   `delete` 拆成两条数，实际它们是同一条端点 `POST /files/objects/{file_id}/delete`。）

impl PartFileService {
    /// 登记一个**已由前端直传到 COS 正式 CAS key** 的对象（INSERT `t_part_file`）。
    ///
    /// 调用方：`POST /api/v2/files/objects/confirm` 的 handler。
    ///
    /// ## 流程（每步的「为什么」见行内注释，错误码即校验顺序）
    ///
    /// 1. 角色闸门 —— `policy::confirm_roles_for(owner_kind, kind)`
    /// 2. owner 存在性 + `owner_kind` × `kind` 配对
    /// 3. 字段校验（kind / sha256 / filename / size / content_type）
    /// 4. **key 防呆**：服务端按 `content_sha256` + `original_filename` **重新派生**
    ///    CAS key，与入参 `object_key` 逐字比对，不一致即 21114
    /// 5. `head_object`：不存在 / size=0 → 21114；size 与声明不符 → 21115
    /// 6. CAS 查重兜底（并发双 confirm）—— 命中活跃行直接复用（配对仍要做）
    /// 7. tx 内：软删旧活跃行 + INSERT + readback
    /// 8. tx 内：CNC 配对（`pair_with_file_id` 给了才做）双向互写
    /// 9. commit → `PartFileOut`
    ///
    /// ## ⚠️ **内容 sha256 不做服务端校验**（与 sha 绑定有关的取舍见 `mod.rs`）
    ///
    /// S3 预签 PUT 走 **UNSIGNED-PAYLOAD**（SigV4 的 `x-amz-content-sha256` 固定
    /// `UNSIGNED-PAYLOAD`），无法把 payload hash 绑进签名 ⇒ 服务端**既拿不到**字节，
    /// 也无法重算 sha。因此 `content_sha256` 是「客户端声明」，服务端只能用两种
    /// 方式约束它：① 用它派生 CAS key 并在步骤 4 比对（保证 key 由 sha 决定，
    /// 不能指向任意对象）；② 同 owner + kind + 同 sha 只能有一条活跃行（步骤 6）。
    /// size 则在**入参**（步骤 3 上限）与 **COS head**（步骤 5 实测）两处各校一次。
    ///
    /// 注：pool 直接传（不是 `&mut tx`）—— `head_object` 是外部 IO，不该被包在
    /// 事务里（事务回滚时 IO 已发生、难恢复）。本方法自管 tx（handler 不开 tx），
    /// 是事务范式的唯一例外（详见模块 doc-comment）。
    #[allow(clippy::too_many_arguments)]
    pub async fn bind_uploaded_file(
        &self,
        pool: &PgPool,
        cos: Arc<dyn CosClient>,
        cfg_upload_prefix: &str,
        cfg_max_file_size: usize,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
        object_key: &str,
        sha256: &str,
        original_filename: &str,
        file_size: i64,
        content_type: &str,
        pair_with_file_id: Option<i64>,
        current: &CurrentUser,
    ) -> Result<PartFileOut, AppError> {
        // 1. 权限：**按 owner_kind + kind 两级分派**
        //    （`policy::confirm_roles_for`：CNC 配对两种 kind 是 M+CNC，其余逐字
        //    走 `write_roles_for(Confirm)`；CUSTOMER ⇒ 仅 Manager）
        current.require_any_role(policy::confirm_roles_for(owner_kind, kind))?;

        // 2. owner 存在性（1 次往返；CUSTOMER 臂含 `parent_id IS NULL` 判据）
        {
            let mut conn = pool.acquire().await?;
            let mut repo: &mut sqlx::PgConnection = &mut conn;
            Self::assert_owner_exists(&mut repo, owner_kind, owner_id).await?;
        }
        // 2b. owner_kind × kind 配对（纯内存）
        Self::assert_owner_kind_matches_kind(owner_kind, kind)?;

        // 3. 字段校验（kind 白名单 / sha256 64hex / filename ≤255 / size / content_type）
        //    ⚠️ `cfg_max_file_size` 由 handler 传 `state.config.cos.max_file_size`，
        //    不在 service 里写死数值。
        validate::check_kind(kind)?;
        validate::check_sha256(sha256)?;
        validate::check_filename(original_filename)?;
        validate::check_file_size(file_size, cfg_max_file_size)?;
        validate::check_content_type(content_type, original_filename)?;
        // ⚠️ 扩展名必须落在**本 kind** 的白名单里：`check_kind` 只判「这个 kind 有
        // 没有白名单」，`check_content_type` 只判「声明的 MIME 与扩展名配不配得上」——
        // 两者都不判「这个 kind 收不收这种扩展名」。缺这道，调用方跳过
        // upload-intents 直接打 confirm 就能给 DRAWING 落一行 `.step`。
        // （upload-intents 侧也判同一张表，见 `grant_upload_intents`。）
        {
            let ext = policy::ext_of(original_filename).ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!("文件缺少扩展名: {original_filename:?}"),
                )
            })?;
            if !policy::allowed_exts(kind).contains(&ext.as_str()) {
                return Err(AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!("kind={kind:?} 不接受扩展名 {ext:?}"),
                ));
            }
        }

        // 3b. CNC 配对目标校验（kind 必须互补）+ 目标行存在性。
        //     排在任何写入之前 ⇒ 配错 kind 时零副作用。
        if let Some(pair_id) = pair_with_file_id {
            let counterpart = policy::cnc_pair_counterpart(kind).ok_or_else(|| {
                AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("kind={kind:?} 不是 CNC 配对文件（G_CODE / SETUP_SHEET），不接受 pair_with_file_id"),
                )
            })?;
            let mut conn = pool.acquire().await?;
            let mut repo: &mut sqlx::PgConnection = &mut conn;
            let target = PartFileRepoTrait::get_by_id(&mut repo, pair_id, false)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_PART_FILE_NOT_FOUND,
                        format!("pair_with_file_id={pair_id} 不存在或已软删"),
                    )
                })?;
            if target.kind != counterpart {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!(
                        "pair_with_file_id={pair_id} 的 kind={:?} 与本行 kind={kind:?} 不互补（应为 {counterpart}）",
                        target.kind
                    ),
                ));
            }
        }

        // 4. **key 防呆**：服务端重新派生 CAS key 并与入参比对。
        //    这是「前端不能拿任意 object_key 来登记」的唯一保证 —— 派生只依赖
        //    (upload_prefix, sha256, original_filename)，三者都过了上面的字段校验。
        let cas_key = cos_key::build_cas_key(cfg_upload_prefix, sha256, original_filename);
        if object_key != cas_key {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_TMP_OBJECT_MISSING,
                format!(
                    "object_key={object_key:?} 与按 (content_sha256, original_filename) 派生的 {cas_key:?} 不一致（须用 upload-intents 响应里回的那个 key）"
                ),
            ));
        }

        // 5. head 校验（外部 IO，在 tx 之外）
        let meta = cos.head_object(&cas_key).await.map_err(|e| {
            AppError::biz(
                code::BIZ_PART_FILE_TMP_OBJECT_MISSING,
                format!("head_object 失败（object_key={cas_key:?}）: {e}"),
            )
        })?;
        if meta.size == 0 {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_TMP_OBJECT_MISSING,
                format!("object_key={cas_key:?} 在 COS 上是 0 字节对象"),
            ));
        }
        if meta.size != file_size {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_SIZE_MISMATCH,
                format!(
                    "object_key={cas_key:?} 客户端声明 size={file_size} 与服务端 head size={} 不一致",
                    meta.size
                ),
            ));
        }

        // 6. CAS 查重兜底（并发双 confirm 撞同一 owner+kind+sha）：
        //    正常流程已在 upload-intents 短路，这里是并发兜底。命中活跃行时
        //    **不重复 INSERT**；若还带 `pair_with_file_id`，配对照做（步骤 8 复用
        //    那一行 id），否则直接返回旧行。
        let cas_hit = {
            let mut conn = pool.acquire().await?;
            let mut repo: &mut sqlx::PgConnection = &mut conn;
            PartFileRepoTrait::get_by_owner_kind_sha(&mut repo, owner_id, kind, sha256).await?
        };
        if let (Some(existing), None) = (&cas_hit, pair_with_file_id) {
            return Ok(Self::render_out(existing, owner_kind));
        }

        // 7./8. tx 内写入（+ 配对）
        let mut tx = pool.begin().await?;
        let mut repo: &mut sqlx::PgConnection = &mut tx;
        let new_id = match &cas_hit {
            // CAS 命中：不软删、不 INSERT，只在原有行上做配对（同一 owner 的同一份
            // 内容已被登记过，再写一行会撞 `uk_t_part_file_single`）。
            Some(existing) => existing.id,
            None => {
                // 单文件 kind（DRAWING / 3D_MODEL / ASSEMBLY_MASTER / CAD_2D /
                // G_CODE / SETUP_SHEET / DELIVERY_TEMPLATE）下，旧活跃行要先
                // soft_delete（`uk_t_part_file_single` 部分唯一约束，谓词按
                // `(part_id, kind)` 走 —— 该列装的就是 owner id，对三种 owner
                // 等价）。这也是送货单打印模板「二次上传 = 替换」的落点。
                let _ = PartFileRepoTrait::soft_delete_active_by_part_kind(
                    &mut repo, owner_id, current.id, kind,
                )
                .await?;
                let id = self.snowflake.next_id();
                let ext = policy::ext_of(original_filename)
                    .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "缺少扩展名"))?;
                let file_type = policy::file_type_for_ext(&ext).ok_or_else(|| {
                    AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("未知扩展名 {ext}"))
                })?;
                PartFileRepoTrait::create_part_file(
                    &mut repo,
                    NewPartFile {
                        id,
                        part_id: owner_id,
                        owner_kind,
                        kind,
                        file_type,
                        object_key: &cas_key,
                        original_filename,
                        file_size,
                        content_type,
                        // ⚠️ 恒 READY：字节在 confirm 之前就已经在 COS 上（head
                        // 校验通过），本行一落库即为可用状态。
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
                        return AppError::biz(
                            code::BIZ_PART_FILE_DUPLICATE,
                            "同 owner + kind 已有活跃行（并发 confirm）",
                        );
                    }
                    AppError::from(e)
                })?;
                id
            }
        };

        // 8. CNC 配对：两行互指（`set_paired_file_id(new → pair)` + `set_paired_file_id(pair → new)`）
        if let Some(pair_id) = pair_with_file_id {
            PartFileRepoTrait::set_paired_file_id(&mut repo, new_id, pair_id, current.id).await?;
            PartFileRepoTrait::set_paired_file_id(&mut repo, pair_id, new_id, current.id).await?;
        }

        let row = PartFileRepoTrait::get_by_id(&mut repo, new_id, true)
            .await?
            .ok_or_else(|| AppError::internal("刚 INSERT 的 part_file 查不到"))?;
        let out = Self::render_out(&row, owner_kind);
        tx.commit().await?;

        Ok(out)
    }
}

// ===== 2026-09-15 takeover-fill：content / delete（Phase 3 补齐） ==========================

/// 后端代理文件二进制流：拉 `object_key` → COS `get_object` → 透传 content_type。
///
/// 权限由**取字节的那条路径**各自判定，本结构只是「字节 + content_type」的载体：
/// - `GET /objects/{id}/content` → [`PartFileService::get_file_content_of_checked_row`]
///   的 5 角色白名单（Manager / Clerk / Inspector / CncProgrammer / **ShelfAccount**，
///   2026-10-10 新增 ShelfAccount —— 报工台工控机账号预览图纸）；
/// - `GET /print-templates/{id}/content` → 权限点 `delivery_note.print`（M / C / I）。
///
/// ⚠️ 与 `get_file_with_url` **同为按 id 取文件的读端点，但守卫与风险面不同**：本端点
/// 由后端代理、经 RBAC 逐次把关、对 ShelfAccount 放行；`/url` 的守卫是权限点
/// `part.drawing.print`（默认矩阵不含 ShelfAccount），回的是 COS 预签直链，拿到即可
/// 脱离本后端直接访问。
///
/// ⚠️ 2026-10-16 review 第 2 轮 MINOR-A：本结构原先的 doc 把「5 角色」叙述成
/// **本文件里那个已删的 `get_file_content` 方法**的权限故事，而那个方法正好是
/// review 第 1 轮 MAJOR-2 关掉的那条「有角色闸门、**无 kind 闸门**」的绕行面，
/// 以 `pub` 形态留在原地（生产侧零调用方，只剩两个测试在调）⇒ 本轮**整条删除**。
/// 现在「按 id 取字节」在本域只有两条合法入口，都**先判 kind 再取字节**。
///
/// ⚠️ 「有角色闸门」这半句**不产生任何拒绝**（2026-10-16 review 第 3 轮 NIT-4 订正）：
/// 它要的白名单是 Manager / Clerk / Inspector / CncProgrammer / ShelfAccount 共 5 个
/// 角色，而 `require_any_role` 对 `Role::Admin` 元角色短路（`has_role(Admin)` 恒真，
/// 见 `src/auth/rbac.rs`）⇒ **全部 6 个角色都放行**。所以删除那个方法的**真正理由**
/// 只有「**无 kind 闸门**」那半句（那半句完全成立）：它按 id 取字节、不过滤
/// `DELIVERY_TEMPLATE`，正是第 1 轮 MAJOR-2 关掉的那条越权读链。
pub struct PartFileContent {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

impl PartFileService {
    /// `GET /api/v2/files/objects/{file_id}/content` 的 service 形态：
    /// **角色闸门 + 取字节**，行由调用方取好（判 kind 之后）传进来。
    ///
    /// ⚠️ **本方法刻意不判 kind** —— 判据在调用方（`handler::get_part_file_content`
    /// 判 `policy::is_file_content_kind`，不通过 ⇒ **21102**，且排在取字节**之前**
    /// ⇒ 非法 kind 零外部 IO）。⚠️ 别把它改成「顺手在这里判一下」：本域有**两个**
    /// 取字节的调用方，各自定义 kind 白名单与错误码（files 域 21102 vs
    /// `print_template` 域 21101），混进来就要开布尔开关（见
    /// [`Self::get_file_content_for_row`] 的 doc）。
    ///
    /// 2026-09-29 扁平化：与 `get_file_with_url` 同款 legacy→新模板 fallback，
    /// 详见 `resolve_effective_key`。
    ///
    /// ## ⚠️ 为什么是「已取到行」的形态（review 第 1 轮 MINOR-5）
    ///
    /// 原先这里是「handler 取一次行判 kind → service 又 `get_by_id` 一次」⇒
    /// **同一行同一谓词查了两遍**（`print_template::service::get_template_content`
    /// 同款），两次读之间还有一行被软删的窗口。拆成
    /// [`Self::get_file_content_for_row`]（**不判权限、不查库**，判据全归调用方）
    /// + 本方法（本域角色闸门）后，一行只取一次。
    ///
    /// 权限（2026-10-10 新增 `Role::ShelfAccount`）：与 `list_files` 同一套 5 角色。
    /// 报工台（`/scan/*`）跑的就是 `SHELF_ACCOUNT` 账号，其图纸预览打的是本端点
    /// （后端代理二进制流）—— 不放开则预览必然 403（业务码 40300）。
    pub async fn get_file_content_of_checked_row(
        &self,
        cos: &dyn CosClient,
        cfg_upload_prefix: &str,
        row: &TPartFile,
        current: &CurrentUser,
    ) -> Result<PartFileContent, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
            Role::ShelfAccount,
        ])?;
        self.get_file_content_for_row(cos, cfg_upload_prefix, row)
            .await
    }

    /// 「按**已取到的行**取字节」—— 本域取字节的**最底层**原语
    /// （2026-10-16 review 第 1 轮 MINOR-5 新增）。
    ///
    /// ⚠️ 本方法**刻意不判权限、不判 kind、不查库**：判据全部由调用方给
    /// —— 两条调用方要的东西不同，混进本方法会逼出一串布尔开关：
    /// - `files::part_file::handler::get_part_file_content`：角色 = 本域 5 角色
    ///   （经 `get_file_content_of_checked_row`）、kind = `policy::is_file_content_kind`
    ///   白名单（不通过 ⇒ **21102**）；
    /// - `print_template::service::get_template_content`：角色 = 权限点
    ///   `delivery_note.print`（M / C / I，**不在**本域 5 角色那张表里 ——
    ///   一个只持该权限点的账号不该被本域的角色白名单二次拒绝）、kind = 「必须是
    ///   `DELIVERY_TEMPLATE`」（不通过 ⇒ **21101**，前端据此刷新列表）。
    pub async fn get_file_content_for_row(
        &self,
        cos: &dyn CosClient,
        cfg_upload_prefix: &str,
        row: &TPartFile,
    ) -> Result<PartFileContent, AppError> {
        let effective_key =
            Self::resolve_effective_key(cos, cfg_upload_prefix, &row.object_key).await?;
        let bytes = cos.get_object(&effective_key).await?;
        Ok(PartFileContent {
            bytes,
            content_type: Some(row.content_type.clone()),
        })
    }

    /// `POST /api/v2/files/objects/{file_id}/delete`。
    ///
    /// 权限：按 `kind` 派生（DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET /
    /// **ASSEMBLY_MASTER** → M+C；G_CODE → M+CNC）。
    ///
    /// 行为：乐观锁守；UPDATE `deleted_at = now()` + `version = version + 1`。
    ///
    /// 返回软删行的 `object_key`，由 **handler 在 `tx.commit()` 之后** 异步
    /// `tokio::spawn(cos.delete_object(...))`——避免 commit 失败却已触发
    /// COS 删除、孤儿对象风险（2026-09-15 review 第 1 轮 A2 修）。
    ///
    /// ### ⚠️ 2026-10-13 review 第 1 轮 MINOR-6：`ASSEMBLY_MASTER` 补进分派
    ///
    /// 分派表原先**不含** `ASSEMBLY_MASTER`，于是「能被上传、永远删不掉」的
    /// `t_part_file` 行是造的（`POST /files/objects` 与 PR1b 之前的
    /// `POST /assemblies/{id}/files` 两条老入口都收 `kind=ASSEMBLY_MASTER`，
    /// 删除侧只认其余 4 种）。该缺口在 `master` 上就存在（分派逐字相同）、
    /// PR1b 未改动删除路径，**不是本 PR 的回归**；但 PR1b 把 files 域变成
    /// `t_part_file` 的**唯一**写入域之后，「写得了、删不了」成了**该域自身**的
    /// 缺陷，故本轮闭合。
    ///
    /// **零角色面变化**：并入的是既有那一支 `M + C`（Manager / Clerk），而
    /// `ASSEMBLY_MASTER` 行本来就只有 Manager / Clerk / CncProgrammer 能造出来
    /// （上传端点白名单 M+C+CNC）——没有引入任何新角色，对 CNC 只是补上
    /// 「自己能造的行自己能删」。⚠️ 不要再拿「会放宽写端点角色面」当不改的理由：
    /// 那是把「新增一个角色」与「新增一个 kind 落进既有角色的桶」混为一谈。
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
            "DRAWING" | "3D_MODEL" | "CAD_2D" | "SETUP_SHEET" | "ASSEMBLY_MASTER" => {
                current.require_any_role(&[Role::Manager, Role::Clerk])?;
            }
            "G_CODE" => {
                current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
            }
            // 2026-10-16 新增：送货单打印模板**只放 MANAGER**。
            // ⚠️ 不加这一支就是「写得进、删不掉」——`owner_kind_allows_kind`
            // 放行了 `CUSTOMER + DELIVERY_TEMPLATE`，所以 CLERK 能通过
            // `POST /files/objects` 造出这种行，而本分派表若缺它，别的角色一律
            // 撞 `other => 不可软删` 分支 ⇒ 模板永久删不掉（`print_template` 域的
            // `POST /print-templates/{id}/delete` 也走本方法，它自己已是
            // MANAGER-only，与这一支同面）。与 ASSEMBLY_MASTER 那次
            // （2026-10-13 review 第 1 轮 MINOR-6）同款闭包。
            "DELIVERY_TEMPLATE" => {
                current.require_any_role(&[Role::Manager])?;
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!(
                        "kind={other:?} 不可软删（仅 DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET / ASSEMBLY_MASTER / G_CODE / DELIVERY_TEMPLATE）"
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
