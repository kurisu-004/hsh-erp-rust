//! part_file 域业务逻辑（2026-09-14 Phase 3 + 2026-09-16 M2-B 业务层 + 2026-09-18 upload-intents 删除 + 2026-09-22 对齐 iam 范式）
//!
//! 对应 Python myERP/service/part_file_service.py + service/_file_kind_policy.py。
//!
//! ## 上传核心逻辑（`upload_file_for_owner`）
//! 0. `owner_kind` × `kind` 配对校验（`policy::owner_kind_allows_kind`；2026-10-13
//!    review 第 1 轮 MINOR-3 补）—— 四合一之前这层配对由各端点的 path 段结构性保证，
//!    合并后两个字段都来自 client ⇒ 服务端重新强制
//! 1. 取 `owner_kind`（PART / ASSEMBLY），校验存在性（21105 OWNER_NOT_FOUND）—— 走
//!    `repo.part_owner_exists` / `repo.assembly_owner_exists`
//!    - **空字节 / 超限守卫**（2026-10-13 review 第 1 轮 MAJOR-1 补）：0 字节 → 40001；
//!      超 `cfg_max_file_size`（handler 传 `state.config.cos.max_file_size`）→ 21103。
//!      这两道是被删的 `PartService::upload_part_file` 带过的，PR1b 合并时漏了
//!    - **`owner_kind` × `kind` 配对**（MINOR-3 补，见步骤 0）
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
//! ## 与 part / assembly 域的耦合
//! 2026-10-13 PR1b 起**恒为「零」**：两条上传 / 一条 confirm 端点全部收进本子模块
//! （`POST /api/v2/files/objects` 四合一 + `POST /api/v2/files/objects/confirm`），
//! part 域与 assembly 域不再持有任何 `t_part_file` 的写入路径。
//! 装配体 PDF（`kind='ASSEMBLY_MASTER'`）走同一条通道，`owner_kind='ASSEMBLY'`。
//! ⚠️ `POST /api/v2/parts/batch` 的 bindings 分支（`service/batch.rs::
//! prepare_binding_head_copy`）**刻意留在 part 域**：它的端点本体是「批量建零件」，
//! 文件只是可选入参，不是一条独立的文件端点。
//!
//! ## 2026-10-13 PR1b：COS key 前缀统一 `obj/`
//! 上传写入的 CAS key 前缀取自 `cfg.cos.upload_prefix`（缺省 `obj/`），读端点的
//! legacy fallback 用**同一份**配置。⚠️ **不需要跑 `cos_key_migrate` 做数据迁移**：
//! `t_part_file.object_key` 存的是完整 key，`resolve_effective_key` 先
//! `head_object(db_object_key)`、命中就直接返回 ⇒ 历史上落在 `drawings/` 或桶根的
//! 对象原地不动，本轮只是让**新上传**统一落 `obj/`。
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

use sqlx::PgPool;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::files::part_file::dto::{PartFileListQuery, validate};
use crate::modules::files::part_file::model::TPartFile;
use crate::modules::files::part_file::policy;
use crate::modules::files::part_file::repo::{NewPartFile, PartFileRepoTrait, hash_bytes};
use crate::modules::files::part_file::vo::{PartFileListOut, PartFileOut, PartFileWithUrlOut};
use crate::shared::error::{AppError, code};
use crate::util::cos_key;

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

    /// 单文件上传（multipart `file` 字段 + JSON `data` 字段已解析）。
    ///
    /// `original_filename` 是客户端 multipart 的 filename；`content_type` 是
    /// 客户端声明的 MIME；`bytes` 是文件二进制。
    ///
    /// 返回新建的 `PartFileOut`（含 id）。
    ///
    /// ## 2026-10-13 PR1b：本方法是**全仓唯一的上传实现**
    ///
    /// 旧的 5 套并存的写法（part 域 `PartService::upload_part_file` /
    /// `upload_drawing` / `upload_3d_model` + assembly 域
    /// `AssemblyService::upload_assembly_files` + PR1a 下线的
    /// `POST /api/v2/part-files/`）全部并到本方法，四者收敛为**一个** HTTP 端点
    /// `POST /api/v2/files/objects`。详见 `handler.rs` 的端点 doc。
    ///
    /// ## ⚠️ 与被删的 `PartService::upload_part_file` 的去重语义差异（本轮裁决）
    ///
    /// | | `upload_part_file`（已删） | 本方法（保留） |
    /// |---|---|---|
    /// | 去重判据 | `PartFileRepo::get_by_part_kind(owner, kind)` —— **不看内容**，命中即复用旧行 | `get_by_owner_kind_sha(owner, kind, sha)` —— 看内容，**同内容**才复用 |
    /// | 同 owner+kind 换内容 | 复用旧行并 `PUT` 新对象（DB 仍指旧 key）⇒ 读到的可能是旧内容 | 落新行；撞 `uk_t_part_file_single` 时由 service 映射成 21108 |
    /// | 同内容不同文件名 | 复用旧行 | 复用旧行（同 sha），返回旧 `original_filename` |
    ///
    /// 取舍：**以本方法为准**（它是 files 域 canonical，且 sha 去重能真正省掉一次
    /// COS PUT）。被判据 `uk_t_part_file_single` 兜住的是「一个 owner 一个 kind 只有
    /// 一条活跃行」，两条实现的这条不变式其实一致，差别只在**撞的是哪种情况**时报什么。
    ///
    /// ## ⚠️ 2026-10-13 review 第 1 轮 MAJOR-1：文件大小与空字节两道守卫
    ///
    /// 本方法此前**没有**任何 size 判据（PR1b 合并时把被删的
    /// `PartService::upload_part_file` 里的两道守卫一起丢了）。后果有三条：
    /// - 有效上限从 `cfg.cos.max_file_size`（`.env.example` = 100MB）放宽到
    ///   `max_request_body_size`（300MB）且改由全局 layer 兜底；
    /// - 0 字节文件能一路过 ext / content_type 两关 → `put_object` 写 0 字节对象
    ///   + INSERT `file_size = 0` 的行；
    /// - `21103 BIZ_PART_FILE_TOO_LARGE` 在 multipart 上传路径上彻底不可达。
    ///
    /// `cfg_max_file_size` 由 handler 传 `state.config.cos.max_file_size`
    /// （与被删实现同源）；**不要**在 service 里写死数值。守卫位置在「owner 存在性」
    /// 之后 —— 错误码优先级即校验顺序：`40300`（角色）→ `20104`（owner_kind × kind
    /// 配对）→ `21105`（owner 不存在）→ `40001`（空字节）→ `21103`（超限）→
    /// `21102`（扩展名 / content_type）。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file_for_owner<R: PartFileRepoTrait>(
        &self,
        mut repo: R,
        cfg_upload_prefix: &str,
        cfg_max_file_size: usize,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
        original_filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
        current: &CurrentUser,
    ) -> Result<PartFileOut, AppError> {
        // 角色闸门（2026-10-16 新增 CUSTOMER 分支，review 第 1 轮 MAJOR-1
        // 抽到 `policy::write_roles_for` 与 confirm 共用）。
        //
        // PART / ASSEMBLY 保持原集合**逐字不变**（M / C / CNC —— 三条旧上传端点的
        // 并集，见本文件模块 doc 的「唯一一处权限面放大」一节）；CUSTOMER
        // （送货单打印模板）**只放 MANAGER**。
        //
        // ⚠️ **这道分派是「CLERK 不能偷偷绑模板」的收口点之一，且必须与
        // `bind_uploaded_file`（`POST /files/objects/confirm`）共用同一张表**：
        // `owner_kind` 与 `kind` 都由 client 在 multipart `data`（confirm 则是 body）
        // 里给，而 `print_template` 域的 `POST /print-templates` 有自己的
        // MANAGER-only handler 守卫 —— 但那只守**那一个端点**。CLERK 若能直接打
        // `POST /files/objects` 或 `POST /files/objects/confirm` 并写
        // `{"owner_kind":"CUSTOMER","kind":"DELIVERY_TEMPLATE"}`，就能绕过模板管理页
        // 给自己日常对接的客户乱换打印模板（改的是别人出货单上的抬头、地址、抬头行
        // 数与顺序）。所以「谁能写 DELIVERY_TEMPLATE」必须在**共用上传原语**里判，
        // 且两条写入口都要经过它 —— 2026-10-16 首版只在本方法加了 CUSTOMER 分支、
        // 漏了 confirm 那条，review 第 1 轮 MAJOR-1 已闭合（共用函数见
        // `policy::write_roles_for`）。
        // ⚠️ `WriteEntry::Upload` 而**不是**传 `policy::UPLOAD_ROLES`（review 第 2 轮
        // MINOR-B）：入口由类型表达，调用点传不出「错的常量集合」——
        // 那是静默放开 confirm 的 CNC 面那种改动的温床。
        current.require_any_role(policy::write_roles_for(
            owner_kind,
            policy::WriteEntry::Upload,
        ))?;

        // 0. owner_kind × kind 配对（2026-10-13 review 第 1 轮 MINOR-3）：四合一
        //    之前这层配对由「各端点自己的 path 段 + 硬编码 kind」在结构上保证，
        //    合并后两个字段都来自 multipart `data` ⇒ 必须由服务端重新强制。
        //    纯内存判断，放在最前面（不花一次 DB 往返）。
        Self::assert_owner_kind_matches_kind(owner_kind, kind)?;

        // 1. owner 存在性校验（走 trait helper）
        Self::assert_owner_exists(&mut repo, owner_kind, owner_id).await?;

        // 1b. 空字节 / 超限守卫（2026-10-13 review 第 1 轮 MAJOR-1 补回）。
        //     **判定与错误码逐字等价**于被删的 `PartService::upload_part_file`
        //     （`bytes.is_empty()` → 40001、`len() > cfg_max_file_size` → 21103），
        //     两处有意的不同，勿当漏改：
        //     ① 消息 token 由 `file_type` 换成 `kind`（旧 "PDF 字节为空" →
        //        新 "DRAWING 字节为空"，信息量更高），超限措辞同理改为报实际
        //        bytes 数 + 上限 MB（旧的 `{file_type} > {max} bytes` 连被超的
        //        实际大小都没报）；
        //     ② 位置后移：被删实现在角色守卫之后紧接这两道，本实现前面还排着
        //        `owner_kind` × `kind` 配对（step 0）与 owner 存在性（step 1），
        //        非法组合先被 20104 / 21105 拦下、不再走到 size 判定。
        //     ⚠️ 空字节**不是**「过大」而是「无效输入」：用 VALIDATION_ERROR (40001)
        //     而不是 BIZ_PART_FILE_TOO_LARGE (21103)。
        if bytes.is_empty() {
            return Err(AppError::validation(format!("{kind} 字节为空")));
        }
        if bytes.len() > cfg_max_file_size {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_TOO_LARGE,
                format!(
                    "{kind} {} bytes 超过上限 {cfg_max_file_size}（{}MB）",
                    bytes.len(),
                    cfg_max_file_size / 1024 / 1024
                ),
            ));
        }

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
        //    2026-10-13 PR1b：prefix 由硬编码 `""` 改为读 `cfg.cos.upload_prefix`
        //    （缺省 `obj/`）—— 上传写入与读端点的 legacy fallback 共用同一份配置。
        let object_key = cos_key::build_cas_key(cfg_upload_prefix, &sha, original_filename);
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
    /// ⚠️ 本端点回的是 **COS 预签直链**（有效期 1 小时），拿到即可绕过本后端任意
    /// 访问、直链可外传。content 不同：字节流由后端代理、每次访问都重新过 RBAC。
    /// 要放开「工控机看图纸」时放开 content 即可，不要顺带把这里的授权也发出去。
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
    /// 把已上传到 COS tmp 区的一个对象绑定到 owner（INSERT t_part_file）。
    ///
    /// 调用方：`POST /api/v2/files/objects/confirm` 的 handler。
    ///
    /// 流程：
    /// 0. 角色守卫（**按 `owner_kind` 派生**：`policy::write_roles_for`；非 CUSTOMER
    ///    是 M+C、逐字沿用旧端点；`CUSTOMER` ⇒ 仅 Manager）
    ///    ＋ **拒 `owner_kind = "CUSTOMER"`**（20104，2026-10-16 review 第 1 轮
    ///    MAJOR-1 补：模板的「替换」不变式只在 `print_template` 域那条路径上，
    ///    见下面步骤 1a 的注释）
    /// 1. owner 存在性校验（`owner_kind ∈ {PART, ASSEMBLY}`；2026-10-13 PR1b 新增，
    ///    旧形态 owner 在 path 段恒 PART、无需校验。⚠️ 步骤 1a 之后 CUSTOMER 已被
    ///    拒 ⇒ 本步实际只判 PART / ASSEMBLY）
    ///    ＋ `owner_kind` × `kind` 配对校验（2026-10-13 review 第 1 轮 MINOR-3）
    /// 2. `tmp_key` 前缀防呆：必须以 `cfg_tmp_prefix` 开头（防客户端乱传 key 读到别人文件）
    /// 3. `head_object` 校验对象存在 + size 与声明一致：
    ///    - 不存在 → `BIZ_PART_FILE_TMP_OBJECT_MISSING` 21114
    ///    - size 不一致 → `BIZ_PART_FILE_SIZE_MISMATCH` 21115
    ///    - 超 `cfg_max_file_size`（handler 传 `state.config.cos.max_file_size`）
    ///      → `BIZ_PART_FILE_TOO_LARGE` 21103（2026-10-13 review 第 1 轮 MAJOR-1：
    ///      原先此处写死 300MB，改由配置下发）    /// 4. 派生 CAS key：`util::cos_key::build_cas_key(prefix, sha16, filename)`
    /// 5. `cos.copy_object(tmp_key, cas_key)` —— 走 PermanentCos（不走 STS）
    /// 6. copy_object 成功后**立刻 spawn** best-effort delete_object(tmp_key) 早期清理
    /// 7. 开 tx：soft_delete 旧活跃行（`uk_t_part_file_single` 部分唯一约束） + INSERT 新行
    /// 8. commit
    /// 9. 返回 `(PartFileOut, tmp_key)` —— caller 拿到 tmp_key 后**已 commit**，再
    ///    spawn 异步 delete_object(tmp_key) 兜底清理
    ///
    /// ⚠️ **`ASSEMBLY_MASTER` 的确认面**（2026-10-13 PR1b 计划外扩大）：`check_kind`
    /// 复用 `policy::allowed_exts`，而该白名单本轮为 multipart 上传新增了
    /// `ASSEMBLY_MASTER` ⇒ confirm 端点**顺带**开始接受这个 kind。四合一的本意
    /// 只是让 assembly 的 PDF 能走上传端点，confirm 侧一并放开是复用的副作用，
    /// 不是设计意图；保留（两条路径对 kind 的接受面一致，是同一张白名单的必然结果），
    /// 但已在 `part_file/mod.rs` 与 `files/mod.rs` 的移除记录里显式登记。
    ///
    /// **重要**：head/copy 是外部 IO，**不**放进事务 tx 里——tx 里只做 DB（事务回滚时
    /// IO 已发生难恢复），依赖 6 / 9 两层 spawn 双层防护。
    ///
    /// 注：pool 直接传（不是 `&mut tx`）—— head/copy 用 pool 直连，确保与 tx 隔离。
    /// 本方法自管 tx（handler 不开 tx），不依赖 handler 借连接——是事务范式的
    /// 唯一例外（详见模块 doc-comment）。
    #[allow(clippy::too_many_arguments)]
    pub async fn bind_uploaded_file(
        pool: &PgPool,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        cfg_upload_prefix: &str,
        cfg_tmp_prefix: &str,
        cfg_max_file_size: usize,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
        tmp_key: &str,
        sha256: &str,
        original_filename: &str,
        file_size: i64,
        content_type: &str,
        current: &CurrentUser,
    ) -> Result<(PartFileOut, String), AppError> {
        // 1. 权限（按 owner_kind 派生，与 multipart 上传端点**共用同一张分派表**）
        //
        // ⚠️ 2026-10-16 review 第 1 轮 MAJOR-1：本方法原先恒是 `M + C`、与
        // `owner_kind` 无关，于是本轮给 `owner_kind_allows_kind` 加
        // `CUSTOMER => DELIVERY_TEMPLATE` 之后，CLERK 打本端点就能造出（并替换掉）
        // **任意未软删客户**名下的打印模板行 —— `assert_owner_exists` 的 CUSTOMER 臂
        // 只判存在（第 1 步那句判据**当时**不判 `parent_id IS NULL`；review 第 2 轮
        // MAJOR-A 已把该谓词下沉进去），第 7 步的
        // `soft_delete_active_by_part_kind` 还会把该客户现有模板软删掉。
        // 修法是两条写入口共用 `policy::write_roles_for`，本方法不再自带一套。
        // ⚠️ 非 CUSTOMER 的默认集合**仍是 M + C**（`WriteEntry::Confirm` ⇒
        // `policy::CONFIRM_ROLES`）—— 刻意不与上传端点的 M+C+CNC 合并，
        // 那会顺带放开本端点的 CNC 面（review 第 2 轮 MINOR-B：入口由类型表达，
        // 传不出错的常量）。
        current.require_any_role(policy::write_roles_for(
            owner_kind,
            policy::WriteEntry::Confirm,
        ))?;

        // 1a. ⚠️ 本端点**不接受 `owner_kind = "CUSTOMER"`**（2026-10-16 review 第 1 轮
        //     MAJOR-1 闭合的第二个缺口）。角色闸门只保证「只有 Manager 能到这里」，
        //     但本端点**没有** `print_template` 域的那条业务不变式：
        //     - 「只绑一级客户（`parent_id IS NULL`）」的强制点在
        //       `print_template::service::l1_customer_name`，本方法**没有**；
        //       ⚠️ 2026-10-16 review 第 2 轮 MAJOR-A 已把该判据**也**下沉进本域
        //       共用的 `customer_owner_exists`（本方法第 1b 步会调到它）⇒ 本端点
        //       其实**也**守得住 L1-only 了。它仍然拒 CUSTOMER 的理由**不变**：
        //       - 「同 L1 至多一份活跃模板 + 先软删后写入」的**替换语义**在那个域里，
        //         本端点没有 —— 经它能造出**第二行**活跃模板吗？不能（唯一索引），
        //         但它会走 `bind_uploaded_file` 自己的软删路径（`part_id` + `kind`），
        //         语义与那个域的「先软删再写」不是同一套；
        //       - 本端点**零个合法 CUSTOMER 调用方**（STS 直传是前端上传大图纸的
        //         路径，模板走 `print_template` 域的一次性 multipart 上传）。
        //     ⇒ 直接拒。⚠️ 错误码 20104 而不是 403：角色已经是 Manager 了，
        //     这里判的是「这个 owner 类型本端点不服务」。
        if owner_kind == "CUSTOMER" {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "owner_kind={owner_kind:?} 不走本端点：送货单打印模板只经 print_template 域的 \
                     POST /print-templates 写入（只有那里有「替换」这条不变式）"
                ),
            ));
        }
        // 1b. owner 存在性（2026-10-13 PR1b 新增：owner 改由 body 携带，见 dto::ConfirmFileIn）
        {
            let mut conn = pool.acquire().await?;
            let mut repo: &mut sqlx::PgConnection = &mut conn;
            Self::assert_owner_exists(&mut repo, owner_kind, owner_id).await?;
        }
        // 1c. owner_kind × kind 配对（2026-10-13 review 第 1 轮 MINOR-3）：
        //     `check_kind` 只认 kind 白名单、不看 owner ⇒ 这里补上另一半，
        //     否则 confirm 也能造出「part + ASSEMBLY_MASTER」这种永不可删的行。
        Self::assert_owner_kind_matches_kind(owner_kind, kind)?;

        // 2. 字段校验（kind/sha/filename/size/content_type）
        // ⚠️ 2026-10-13 review 第 1 轮 MAJOR-1：原先这里是硬编码的
        // `300 * 1024 * 1024`，与 `cfg_upload_prefix` 同源的 `cfg_max_file_size`
        // 形参一起由 handler 传 `state.config.cos.max_file_size` 下来 —— 这是
        // CLAUDE.md 已登记的「`COS_MAX_FILE_SIZE` 解析了从没被读」的同一件事。
        validate::check_kind(kind)?;

        validate::check_sha256(sha256)?;
        validate::check_filename(original_filename)?;
        validate::check_file_size(file_size, cfg_max_file_size)?;
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
        // 单文件 kind（DRAWING / 3D_MODEL / ASSEMBLY_MASTER / CAD_2D）下，旧活跃行
        // 要先 soft_delete（`uk_t_part_file_single` 部分唯一约束，谓词按
        // `(part_id, kind)` 走 —— 2026-10-13 PR1b 起 owner 可能是 ASSEMBLY，但该列
        // 装的就是 owner id，故这行 SQL 对两种 owner 等价）。
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
                owner_kind,
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
        let out = Self::render_out(&row, owner_kind);
        tx.commit().await?;

        Ok((out, tmp_key.to_string()))
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
