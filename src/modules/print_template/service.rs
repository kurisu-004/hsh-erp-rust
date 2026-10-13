//! `print_template` 域业务逻辑（**2026-10-16 新建**）
//!
//! 全部是**自由函数**（范式同 `mail_digest::service` / `statistics::service`），
//! 签名收 `&mut PgConnection`（handler 借 `&mut *tx` / `&mut *conn` 喂进来）——
//! service 不知事务，事务边界在 handler（CLAUDE.md「必须遵守的架构约定」第 1 条）。
//!
//! ## 为什么**没有** repo 层
//!
//! 本域没有自有表。`t_part_file` 的读写全部走 `files::part_file` 已有的
//! `PartFileRepoTrait`（该 trait 已 `impl for &mut PgConnection`，可直接借位调用），
//! `t_customer` 的两处只读查询**直接写在本文件里**（运行时 `sqlx::query_scalar`，
//! **不进 `query!` 宏**、不需要 `.sqlx/` 离线缓存 —— 与 `files::part_file::repo`
//! 的既有取舍一致，见该文件模块 doc 的「约定」一节）。
//!
//! 范本：`files::part_file::cnc` 子模块（「复用 `t_part_file`、无 repo 层」）。
//!
//! ## ⚠️ 上传的**顺序**是被唯一索引逼死的，不是随手写的
//!
//! `uk_t_part_file_single (part_id, kind) WHERE deleted_at IS NULL AND kind IN
//! (…, 'DELIVERY_TEMPLATE')` 表达的是「**一个 L1 至多一份活跃模板**」。
//! 所以「替换」这个语义在 DB 层只能这样实现：
//!
//! ```text
//! ① soft_delete_active_by_part_kind(customer_id, by, "DELIVERY_TEMPLATE")
//!    ② upload_file_for_owner(repo=同一个 &mut PgConnection, …)
//!    ③ commit
//! ```
//!
//! ⚠️ **② 必须在 ① 之后、且必须与 ① 共用同一个 tx**：
//! - 先 INSERT 后软删 ⇒ 撞 `uk_t_part_file_single`（23505）；
//! - 不共用同一个 tx ⇒ ① 成功、② 失败时会留下「该客户**没有**模板」的空洞，
//!   而用户明明只想换一份。
//!
//! 共用 tx 的代价是：`upload_file_for_owner` 内部任何一步校验失败（扩展名 /
//! content_type / COS PUT / INSERT）都会把 ① 一起回滚 ⇒ 语义正确。
//! **残留的孤儿 COS 对象**：tx 回滚后新对象已经 PUT 出去了。
//! ⚠️ 有意**不**加补偿删除 —— 对象 key 是**内容寻址**的
//! （`util::cos_key::build_cas_key` = `{prefix}{sha16}_{safe_filename}`），
//! 同内容重传会 PUT 到**同一个 key**（幂等覆盖），CAS 去重命中时压根不再 PUT。
//! 真正的孤儿只有「传了份内容 X、事务回滚、内容 X 从此没人再传」这一种形态，
//! 代价是一次对象存储写入。登记在 `docs/api/print_template.md` §8.3 第 3 条。
//!
//! ⚠️ **代价：CAS 去重的省流优化在替换语义下取不到**。`upload_file_for_owner`
//! 的第 5 步按 `get_by_owner_kind_sha` 命中就跳过 COS PUT，而那条查询带
//! `deleted_at IS NULL` —— ① 刚把旧行软删了，所以「重传同一份」走的是
//! 「重新 PUT（幂等）+ INSERT 新行」。这也是可以接受的：省下的是一次幂等写。
//!
//! ## ⚠️ COS PUT 发生在事务内（沿用 `files` 域现状，有意登记）
//!
//! `upload_file_for_owner` 的第 6 步（`cos.put_object`）在 handler 开的事务里。
//! 这**不是**本域引入的形态（`POST /files/objects` 逐字同款），本域沿用它而
//! **不**单独改造 —— 改造它要动 `files` 域的共用上传原语（进而影响图纸 /
//! 3D 模型 / 装配件整图三条既有路径），代价与收益不成比例。
//! 对本域可接受：送货单打印模板是 **xlsx**，通常 < 500KB，
//! 而 `COS_MAX_FILE_SIZE`（`.env.example` = 100MB）才是真正的上限 ——
//! 一份超大模板会把连接在事务里占住相应时长。登记在
//! `docs/api/print_template.md` §8.3 第 3 条。

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::modules::files::part_file::model::TPartFile;
use crate::modules::files::part_file::service::{PartFileContent, PartFileService};
use crate::modules::print_template::dto::{OWNER_KIND, TEMPLATE_KIND};
use crate::modules::print_template::vo::{
    PrintTemplateListOut, PrintTemplateOut, PrintTemplateResolveOut,
};
use crate::shared::error::{AppError, code};

// ============================================================================
//  t_customer 的两处只读查询（本域唯一的跨域表引用）
// ============================================================================

/// 该 id 是不是一个**未软删的一级客户**（`parent_id IS NULL`）。
///
/// ⚠️ 这是**绑定粒度「只绑一级客户」的强制点**。返回 `false` 的四种情形
/// （不存在 / 已软删 / 是二级客户）**一律映射成同一个 `20104`**：写侧刻意
/// 不区分它们 —— 前端拿到的是「你选的这个客户 id 不能绑模板」，
/// 而回显的 message 里带上原始 id 让它自己改选，**不要**让前端去猜是
/// 「客户不存在」还是「客户不是一级」（那需要它再发一次请求去查）。
///
/// 为什么**不**直接复用 `PartFileRepoTrait::customer_owner_exists`（只判存在）：
/// 那道闸门在 `upload_file_for_owner` 内部第 1 步，而它排在 COS PUT **之前**、
/// 但排在**本函数之后** —— 本函数先跑，所以绑二级客户时**零外部 IO** 被拒。
/// `customer_owner_exists` 保留为**兜底**（万一有人直调
/// `PartFileService::upload_file_for_owner` 而绕过本域 handler）。
async fn customer_is_l1(conn: &mut PgConnection, customer_id: i64) -> Result<bool, AppError> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM t_customer \
         WHERE id = $1 AND deleted_at IS NULL AND parent_id IS NULL)",
    )
    .bind(customer_id)
    .fetch_one(conn)
    .await?;
    Ok(ok)
}

/// 一级客户名（查不到 / 已软删 ⇒ `None`，**不**报错）。
///
/// 只在 `resolve` 端点用一次（出参 `customer_name`），不做 N+1、不批量。
async fn customer_name(
    conn: &mut PgConnection,
    customer_id: i64,
) -> Result<Option<String>, AppError> {
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM t_customer WHERE id = $1 AND deleted_at IS NULL")
            .bind(customer_id)
            .fetch_optional(conn)
            .await?;
    Ok(name)
}

// ============================================================================
//  端点 1：POST /  上传并替换
// ============================================================================

/// 上传一份 xlsx 模板并**替换**该一级客户当前绑定的模板。
///
/// ⚠️ **调用顺序是契约的一部分**（handler 必须在调本函数**之前**做完这四道轻校验）：
/// ① `bytes.is_empty()` → `40001`；② 扩展名非 xlsx → `21102`；
/// ③ `bytes.len() > cfg_max_file_size` → `21103`；④ 前 4 字节不是
/// `PK\x03\x04` → `21102`。**这四道都在本域自己的 handler 里**（零事务、
/// 零 COS、零 DB），理由与代价见 [`crate::modules::print_template::dto`] 末尾。
///
/// 本函数内部：L1-only 强制点 → 软删旧行 → 复用 `files` 域的
/// `upload_file_for_owner` 写新行。⚠️ **顺序理由见本文件模块 doc 的
/// 「上传的顺序是被唯一索引逼死的」一节**，不要重排。
///
/// 角色：`upload_file_for_owner` 内按 `owner_kind='CUSTOMER'` 分派成
/// **仅 Manager**（2026-10-16 新增的那道闸门，见
/// `files::part_file::service::upload_file_for_owner`），本函数开头**复核**一次
/// —— handler 第一行已有 `require_role(Manager)`，此处是双层守卫。
#[allow(clippy::too_many_arguments)]
pub async fn upload_template(
    part_file_service: &PartFileService,
    conn: &mut PgConnection,
    cfg_upload_prefix: &str,
    cfg_max_file_size: usize,
    customer_id: i64,
    filename: &str,
    content_type: &str,
    bytes: Vec<u8>,
    current: &CurrentUser,
) -> Result<PrintTemplateOut, AppError> {
    // ① 本域唯一的「必须是一级客户」强制点（零外部 IO）
    if !customer_is_l1(conn, customer_id).await? {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "customer {customer_id} 不能绑送货单打印模板：不存在 / 已软删 / 不是一级客户（只绑一级，parent_id IS NULL）"
            ),
        ));
    }

    // ② 角色复核（handler 已守过一次；这里是 service 层的双层守卫）
    current.require_role(crate::auth::rbac::Role::Manager)?;

    // ③ **先**软删旧活跃行（`uk_t_part_file_single` 的硬要求，理由见模块 doc）
    //
    //    ⚠️ `let mut repo: &mut PgConnection = conn;` 是本仓 trait 借位的既有写法
    //    （`PartFileRepoTrait` 直接 `impl for &mut PgConnection`，方法收 `&mut self`
    //    ⇒ 实参要写 `&mut repo`；与 `files::part_file::service::bind_uploaded_file`
    //    第 7 步逐字同款）。
    {
        let mut repo: &mut PgConnection = conn;
        let _ = crate::modules::files::part_file::repo::PartFileRepoTrait::
            soft_delete_active_by_part_kind(&mut repo, customer_id, current.id, TEMPLATE_KIND)
            .await?;
    }

    // ④ 复用 files 域的**全仓唯一**上传实现（CAS key / sha256 / COS PUT /
    //    owner 存在性复核 / content_type 校验全在它里面）。
    //    ⚠️ `repo` 传 `&mut *conn`（同一个 tx ⇒ 与 ③ 共命运，回滚一起回滚）。
    //    ⚠️ 它回的是 `PartFileOut`（含**从形参回显**的 `owner_kind`），本域
    //    **刻意不直接用它**：`owner_kind` 字段是 files 域的偏差登记对象
    //    （第 7 / 9 条），而这里已经知道行 id —— 用 `get_file_row` 复读一次，
    //    从**行上真实字段**渲染本域的 `PrintTemplateOut`。
    let uploaded = part_file_service
        .upload_file_for_owner(
            &mut *conn,
            cfg_upload_prefix,
            cfg_max_file_size,
            OWNER_KIND,
            customer_id,
            TEMPLATE_KIND,
            filename,
            content_type,
            bytes,
            current,
        )
        .await?;

    let row = part_file_service.get_file_row(conn, uploaded.id).await?;
    Ok(PrintTemplateOut::from_row(&row))
}

// ============================================================================
//  端点 3：GET /resolve
// ============================================================================

/// 取该一级客户当前绑定的模板；**未绑定（含已软删）⇒ `21109`**。
///
/// `21109` 的 HTTP 状态码是 **422**（`src/shared/error.rs::status_from_code`
/// 显式登记），语义是「请求本身合法，但这个客户的配置不满足前提」——
/// 前端据此弹「去模板管理页绑定 / 或手动上传兜底」，而不是「你参数错了」。
pub async fn resolve_template(
    conn: &mut PgConnection,
    customer_id: i64,
) -> Result<PrintTemplateResolveOut, AppError> {
    let row: TPartFile = {
        let mut repo: &mut PgConnection = conn;
        crate::modules::files::part_file::repo::PartFileRepoTrait::get_by_part_kind(
            &mut repo,
            customer_id,
            TEMPLATE_KIND,
        )
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED,
                format!("一级客户 {customer_id} 还没有绑定送货单打印模板"),
            )
        })?
    };
    let name = customer_name(conn, customer_id).await?;
    Ok(PrintTemplateResolveOut {
        template: PrintTemplateOut::from_row(&row),
        customer_name: name,
    })
}

// ============================================================================
//  端点 2：GET /  列表
// ============================================================================

/// 模板列表（按 `?customer_id=` 过滤 + 分页，`created_at DESC`）。
///
/// ⚠️ **调用方必须把 `kind` 钉成 `DELIVERY_TEMPLATE`**：`PartFileRepoTrait::
/// list_with_filters` 的 `owner_kind` 形参**不进 SQL**（表上没有 owner 列，
/// 见 `files/part_file/mod.rs` 已知偏差第 9 条），所以 `kind` 谓词是本端点
/// **唯一**的隔离依据 —— 漏传会把零件图纸 / 装配件整图一并列出来。
/// ⚠️ **必须传 `exclude_kind = None`**：该形参是 files 域通用列表端点那道守卫的
/// 入口（`PartFileService::list_files` 传 `Some("DELIVERY_TEMPLATE")`，2026-10-16
/// review 第 1 轮 MAJOR-3）—— 本端点要的**正是**这一种 kind，排除它就把自己
/// 清空了。两条路径互相独立，**不要**「顺手把两边统一成同一张表」。
pub async fn list_templates(
    conn: &mut PgConnection,
    customer_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<PrintTemplateListOut, AppError> {
    let mut repo: &mut PgConnection = conn;
    let (rows, total) =
        crate::modules::files::part_file::repo::PartFileRepoTrait::list_with_filters(
            &mut repo,
            OWNER_KIND,
            customer_id,
            Some(TEMPLATE_KIND),
            None,
            limit,
            offset,
        )
        .await?;
    Ok(PrintTemplateListOut {
        items: rows.iter().map(PrintTemplateOut::from_row).collect(),
        total,
    })
}

// ============================================================================
//  端点 4：GET /{id}/content
// ============================================================================

/// 取模板字节（后端代理）。
///
/// ⚠️ kind 校验在这里（不在 `files` 域的 `/objects/{id}/content`）—— 那条
/// 路由的 kind 白名单（`policy::is_file_content_kind`）**刻意不含**本 kind，
/// 且它的角色集合含 `SHELF_ACCOUNT`（工控机预览图纸）、是 files 域自己的权限面；
/// 本端点用权限点 `delivery_note.print`（M / C / I），是**打印**语义。两者的
/// 角色集合与错误码（那边 21102、这边 21101）都刻意不同。
///
/// ⚠️ 2026-10-16 review 第 1 轮 MINOR-5：行只取**一次**（判 kind 与取字节共用同一行），
/// 原先这里 `get_file_row` 一次、`get_file_content` 内部又 `get_by_id` 一次。
/// 取字节走 files 域最底层的 `get_file_content_for_row`（**不判权限、不判 kind**）
/// —— 权限点由本域 handler 判、CNC_PROGRAMMER 这类「不在 files 域 5 角色表里但持
/// `delivery_note.print`」的账号不该被二次拒绝。
pub async fn get_template_content(
    part_file_service: &PartFileService,
    conn: &mut PgConnection,
    cos: std::sync::Arc<dyn crate::infra::cos::CosClient>,
    cfg_upload_prefix: &str,
    template_id: i64,
) -> Result<PartFileContent, AppError> {
    let row = part_file_service
        .get_file_row(&mut *conn, template_id)
        .await?;
    if row.kind != TEMPLATE_KIND {
        return Err(AppError::biz(
            code::BIZ_PART_FILE_NOT_FOUND,
            format!(
                "part_file {template_id} 不是送货单打印模板（kind={:?}），不提供字节",
                row.kind
            ),
        ));
    }
    part_file_service
        .get_file_content_for_row(cos.as_ref(), cfg_upload_prefix, &row)
        .await
}

// ============================================================================
//  端点 5：POST /{id}/delete
// ============================================================================

/// 软删模板（OCC：0 行 ⇒ `40901 VERSION_CONFLICT`）。
///
/// 复用 `files::part_file::service::soft_delete_file` —— 它自带
/// `WHERE id=$1 AND version=$2 AND deleted_at IS NULL`，并按 `kind` 分派角色
/// （`DELIVERY_TEMPLATE ⇒ 仅 Manager`，2026-10-16 新补的那一支）。
///
/// 返回软删行的 `object_key`：handler **在 `tx.commit()` 之后**再 spawn
/// `cos.delete_object`（`files` 域 A2 修的「commit 在前、spawn 在后」纪律，
/// 避免 commit 失败却已触发 COS 删除）。
pub async fn delete_template(
    part_file_service: &PartFileService,
    conn: &mut PgConnection,
    template_id: i64,
    version: i32,
    current: &CurrentUser,
) -> Result<String, AppError> {
    part_file_service
        .soft_delete_file(&mut *conn, template_id, version, current)
        .await
}
