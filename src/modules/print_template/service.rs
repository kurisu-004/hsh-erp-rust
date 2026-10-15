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
//! `t_customer` 的三处只读查询**直接写在本文件里**（运行时 `sqlx::query_scalar` /
//! `sqlx::query_as`，**不进 `query!` 宏**、不需要 `.sqlx/` 离线缓存 —— 与
//! `files::part_file::repo` 的既有取舍一致，见该文件模块 doc 的「约定」一节；
//! 逐条清单与「取数恒 ≤ 3 条语句」的护栏见文末两节）。
//!
//! 2026-10-15：上传端点下线 ⇒ `t_customer` 的只读查询由 3 条降为 **2 条**
//! （`customer_name` / `customer_names`；L1-only 判据并入 files 域共用闸门）。
//!
//! 范本：同域的其它「复用 `t_part_file`、无 repo 层」写法。
//!
//! ## ⚠️ 替换语义由 `files::part_file::service::bind_uploaded_file` 承接
//!
//! 「一个 L1 至多一份活跃模板」由 `uk_t_part_file_single (part_id, kind)
//! WHERE deleted_at IS NULL AND kind IN (…, 'DELIVERY_TEMPLATE')` 表达，
//! 所以「先软删旧行、再 INSERT 新行」**必须**在同一个 PG 事务里 ——
//! `POST /files/objects/confirm` 正是这么做的（`bind_uploaded_file` 步骤 7）。
//! 先 INSERT 后软删会撞 23505；不共用 tx 会留下「该客户没有模板」的空洞。
//!
//! ⚠️ 模板行的写入**全部**经 confirm 一条路：本域不再有任何收字节的端点，
//! 也就不存在「两条写入口角色口径分叉」的问题（CUSTOMER × DELIVERY_TEMPLATE
//! 的角色闸门在 `policy::write_roles_for`，confirm 与 upload-intents 共用）。
//!
//! ## ⚠️ 取数恒 ≤ 3 条语句（列表端点，2026-10-16 review 第 2 轮 C-1 补）
//!
//! 端点 2（列表）是 N 行模板的端点，而 `customer_name` 是前端 `.strict()` schema
//! 的**必填**字段（review 第 2 轮 C-1 补的出参）⇒ 天然 tempt 人写成「逐行查名」。
//! 本域的形态：
//!
//! | 端点 | 语句数 | 构成 |
//! |---|---|---|
//! | 2 `GET /` | **3**（`rows` 为空时 2） | `list_with_filters` 的 rows 1 + total 1 + `customer_names` 的 `id = ANY($1)` 1 |
//! | 3 `GET /resolve` | 2 | `get_by_part_kind` 1 + `customer_name` 1 |
//! | 4 `GET /{id}/content` | 1 | `get_file_row` 1（字节走 COS，不算 SQL） |
//! | 5 `POST /{id}/delete` | 2 | `soft_delete_file` 内的 `get_by_id` 1 + UPDATE 1 |
//!
//! ⚠️ **每个数字都与「返回行数 / 模板大小 / 客户数」无关**。⚠️ 这些计数会随
//! files 域的共用原语演化而过期
//! —— **要确切数字就跑一次或读 `files::part_file::service` 的步骤注释**，
//! 别把它当契约；真正不许破的是「与行数无关」这一条，由本文件末尾的
//! `mod query_guard` 源码级护栏钉住（逐行查的写法功能完全正确、集成测试照样全绿，
//! 只有语句数会线性涨）。

use std::collections::HashMap;

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
//  t_customer 的三处只读查询（本域唯一的跨域表引用）
// ============================================================================

// 2026-10-15：`l1_customer_name`（本域上传端点的「只绑一级客户」强制点）随
// 上传端点一并删除 —— 模板行的写入现在只经 `POST /files/objects/confirm`，而
// 「是不是一级客户」这条判据已在 files 域的 `repo::customer_owner_exists`
// （`parent_id IS NULL`）里，是 upload-intents 与 confirm **共用**的那道闸门。
// ⚠️ 错误码随之是 **21105**（OWNER_NOT_FOUND）而不是本域原报的 20104。

/// 一级客户名（查不到 / 已软删 ⇒ `None`，**不**报错）。
///
/// 与 [`l1_customer_name`] 的差别**刻意只有 `parent_id IS NULL` 一个谓词**：端点 3
///（resolve）是**只读**的、模板行已经存在，它要的是「这个 id 对应的客户叫什么」；
/// 万一该客户的层级事后被改（`parent_id` 被填上），打印对话框**不应该**因此打不开
/// —— 那会把一次主数据调整放大成「这条客户全部打不出送货单」。
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

/// 一批一级客户的名称（`id → name`；软删的**不在**结果里，调用方取不到即 `None`）。
///
/// ⚠️ **这是列表端点唯一的客户名来源，刻意用 `id = ANY($1)` 一条取回**（与本仓
/// `GET /com/delivery/note/scan/{serial_no}` 取整棵树同款手法）——
/// **绝不**在 `for` 循环里逐行查，那正是本仓明令禁止的 N+1（见本文件模块 doc 的
/// 「取数恒 ≤ 3 条语句」一节 + 末尾的源码级护栏单测）。
/// `ids` 为空 ⇒ **不发语句**（直接返空表）⇒ 列表端点的语句数因此是「有行 3 条、
/// 空结果 2 条」，与工单数无关。
async fn customer_names(
    conn: &mut PgConnection,
    ids: &[i64],
) -> Result<HashMap<i64, String>, AppError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM t_customer WHERE id = ANY($1) AND deleted_at IS NULL")
            .bind(ids)
            .fetch_all(conn)
            .await?;
    Ok(rows.into_iter().collect())
}

// ============================================================================
//  端点 1：POST /  —— 2026-10-15 起**不存在**（见下方说明）
// ============================================================================

// ⚠️ **本域不再有上传端点**（2026-10-15）
//
// 模板上传改走全仓统一的收文件链路：
// `POST /files/objects/upload-intents`（`owner_kind=CUSTOMER` + `kind=DELIVERY_TEMPLATE`）
// → 前端直传 COS → `POST /files/objects/confirm` 落行。
//
// 替换语义（一个 L1 至多一份活跃模板）由 confirm 内部的「软删旧行 + INSERT」
// 在同一事务里承接（`uk_t_part_file_single` 的硬要求），本域只保留
// 列表 / resolve / 取字节 / 软删四个端点。
//
// ⚠️ **「xlsx 内容是不是真 xlsx」不再由后端校验**：confirm 拿不到字节（只有 head），
// ZIP magic / 模板结构的校验职责移交前端（前端已用 hucre 逐格校验）。
// 登记在 `docs/api/print_template.md`。

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
        // ⚠️ 行内的 `customer_name` 与顶层那个**刻意重复**（2026-10-16 review 第 2
        // 轮 C-1）：前端 schema 是 `.strict()` 且必填行内那个，删任何一个都会破前端。
        template: PrintTemplateOut::from_row(&row, name.clone()),
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
///
/// ## 取数恒 ≤ 3 条语句，与工单数无关（2026-10-16 review 第 2 轮 C-1）
///
/// 本函数**恒**恰好 3 条语句：`list_with_filters` 的 rows 1 条 + total 1 条
/// ＋ [`customer_names`] 批量取客户名 1 条。⚠️ `rows` 为空时 [`customer_names`]
/// 不发语句 ⇒ 那时是 2 条。**与返回行数无关** —— 客户名是**一条
/// `id = ANY($1)`** 一次取回后在内存里 join（`HashMap` 查表），**绝不**在
/// `rows.iter()` 里逐行查。
///
/// ⚠️ 这条**只能靠源码级护栏钉**：逐行查的写法功能上完全正确、集成测试照样全绿，
/// 只有语句数会随行数线性涨。护栏见本文件末尾的 `mod query_guard`。
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
    // ⚠️ 下面三行**刻意不在任何循环里**：批量取名 + 内存 join（C-1）。
    let ids: Vec<i64> = rows.iter().map(|r| r.part_id).collect();
    let names = customer_names(&mut *conn, &ids).await?;
    Ok(PrintTemplateListOut {
        items: rows
            .iter()
            .map(|r| PrintTemplateOut::from_row(r, names.get(&r.part_id).cloned()))
            .collect(),
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
/// 原先这里 `get_file_row` 一次、files 域的 `get_file_content` 内部又 `get_by_id` 一次
/// （那个方法已于 review 第 2 轮 MINOR-A 整条删除）。
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

// ============================================================================
//  源码级护栏：列表端点不许 N+1（2026-10-16 review 第 2 轮 C-1 补）
// ============================================================================

#[cfg(test)]
mod query_guard {
    //! 「列表恒 3 条语句、与行数无关」这件事**只能靠源码护栏守** ——
    //! 逐行查客户名的写法**功能完全正确**（每个名字都对）、集成测试照样全绿，
    //! 只有语句数会随行数线性涨，且退化形态要等客户模板多起来才显形。
    //!
    //! 同款护栏在本仓有先例：`mail_digest` 的 `n_plus_one_guard.rs`（集成测试
    //! binary）与 `shared::domain_guard.rs`（lib 单测读源码）。本域选后者 ——
    //! 本域**没有 repo 层**、`t_customer` 的两条只读查询就在本文件里，
    //! `include_str!` 读自己最短。
    //!
    //! ⚠️ **精度边界（已登记，别当「完备」用）**：三条用例**各自**的漏报形态：
    //! - `no_query_inside_any_loop` 只认「循环体内**直接**出现 sqlx 查询调用」。
    //!   「循环里调一个**别的函数**、那个函数里发查询」它看不见；
    //! - 这种**跨函数的间接 N+1** 由 `list_templates_joins_customer_names_in_memory`
    //!   兜：它显式禁止列表端点调单条取名的 `customer_name(`、并要求出现
    //!   `customer_names(` + `let ids: Vec<i64>` + `names.get(` 三步。
    //!   ⚠️ **这个兜底只覆盖 `list_templates` 一个函数**（2026-10-16 review 第 3 轮
    //!   MINOR-3 补）—— 那条断言取的是 `fn_body("list_templates")`。在**别的**函数里
    //!   逐行查名它一条都拦不住：将来在本文件新增第 6 个端点（哪怕只是导出）并在
    //!   其中写 `for row in rows { customer_name(conn, row.part_id).await? }`，
    //!   第 1 条（循环体内无 `sqlx::query`，那条在别的 helper 里）过、第 2 条（总查询
    //!   点数仍是 3）过、第 3 条（只查 `list_templates`）也过 ⇒ **三条全绿**。
    //!   ⇒ 其它函数里的逐行查名，只能靠「新增 helper 时同步扩断言」这条人工纪律守。
    //! - 若日后新增**第三个**会发查询的 helper 并在别处逐行调用它，这三条都拦不住
    //!   ⇒ 新增 helper 时请顺手扩 `list_templates_joins_customer_names_in_memory`。
    //!
    //! ⚠️ **变异验证的覆盖面（2026-10-16 review 第 3 轮 MINOR-3 订正）**：原文写
    //! 「两条断言（循环内查询数、三条 helper 各一条）都经过变异验证」，其中对**第 1 条
    //! （循环内查询数）不成立** —— `code()` 扫的那段源码里**零个** `for` / `while` /
    //! `loop`（实测：护栏 banner 之前 429 行，剥掉注释后零个迭代语句），`loop_bodies()`
    //! 返回空、自检 `!bodies.is_empty() || !code().contains("\n    for ")` 因 `bodies`
    //! 为空而**恒真** ⇒ 第 1 条**当前不承载任何判定**（一旦有人在本文件写出循环即生效）。
    //! 真正经过**变异验证**（手改成 N+1 后当场红）的只有**第 2 条**（三条 helper 各
    //! 恰好一条查询）与**第 3 条**（列表端点必须走批量那条）。

    /// 护栏模块自身的 banner 行（**只在这一行出现**，下面的 `const` 声明刻意不含
    /// `//  ` 前缀），用于把本模块从源码统计里切掉 —— 断言里含 `"sqlx::query"`
    /// 字面量，不切掉会把自己算进去。
    const GUARD_MARKER: &str = "//  源码级护栏：列表端点不许 N+1";

    /// 被检源码 = 本文件源码**去掉护栏模块**。
    fn code() -> &'static str {
        include_str!("service.rs")
            .split_once(GUARD_MARKER)
            .map_or_else(
                || panic!("service.rs 里找不到护栏 banner（GUARD_MARKER 与源码不同步？）"),
                |(before, _)| before,
            )
    }

    /// 抽出每个 `for` / `while` / `loop` **循环体**的文本（逐行花括号配平）。
    fn loop_bodies(src: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut cur: Option<(i32, String)> = None;
        for line in src.lines() {
            if let Some((depth, buf)) = cur.as_mut() {
                buf.push('\n');
                buf.push_str(line);
                *depth += line.matches('{').count() as i32;
                *depth -= line.matches('}').count() as i32;
                if *depth <= 0 {
                    out.push(std::mem::take(buf));
                    cur = None;
                }
                continue;
            }
            let t = line.trim_start();
            if !(t.starts_with("for ") || t.starts_with("while ") || t.starts_with("loop")) {
                continue;
            }
            let depth = line.matches('{').count() as i32 - line.matches('}').count() as i32;
            if depth <= 0 {
                // 单行循环（`for … { … }` 写在一行内）：整个行就是循环体
                out.push(line.to_string());
            } else {
                cur = Some((depth, line.to_string()));
            }
        }
        if let Some((_, buf)) = cur {
            // 未配平（源码被改坏）也收进来 —— 宁可误报不可漏报
            out.push(buf);
        }
        out
    }

    /// 抽出某个 `async fn <name>(` 的函数体（到下一个行首 `}` 为止）。
    ///
    /// ⚠️ 匹配的是 `"fn <name>("` 而**不是** `"pub async fn <name>("` ——
    /// 本文件里三条取名 helper 都是**私有** `async fn`（只有 `list_templates` 是
    /// `pub`），写成 `pub` 会让三条 helper 一条都找不到（本护栏第一版踩过）。
    fn fn_body(name: &str) -> String {
        let src = code();
        let start = src
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("service.rs 里找不到 fn {name}"));
        let rest = &src[start..];
        let end = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{name} 的函数体没有以行首 `}}` 结束"));
        rest[..end].to_string()
    }

    /// 数「**真正发起查询的调用点**」：`sqlx::query(…)` / `sqlx::query_scalar(…)` /
    /// `sqlx::query_as(…)`（可带 turbofish `::<…>`）。
    ///
    /// ⚠️ **不能**直接 `matches("sqlx::query").count()` —— 那是**前缀**，模块 doc 里
    /// 「`sqlx::query_scalar` / `sqlx::query_as`」那句也会被数进去（本护栏第一版就
    /// 因此把 3 数成了 5）。要求后跟 `(` 才算调用点。
    fn query_calls(src: &str) -> usize {
        let needle = "sqlx::query";
        let mut n = 0usize;
        let mut rest = src;
        while let Some(i) = rest.find(needle) {
            let after = &rest[i + needle.len()..];
            // 吃掉标识符后缀（`_scalar` / `_as`）
            let ident_len = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            let mut tail = &after[ident_len..];
            // 吃掉可能的 turbofish `::<…>`（本文件当前没有，但别让护栏被它骗）
            if let Some(t) = tail.strip_prefix("::<")
                && let Some(close) = t.find('>')
            {
                tail = &t[close + 1..];
            }
            if tail.trim_start().starts_with('(') {
                n += 1;
            }
            rest = &rest[i + needle.len()..];
        }
        n
    }

    #[test]
    fn no_query_inside_any_loop() {
        let bodies = loop_bodies(code());
        assert!(
            !bodies.is_empty() || !code().contains("\n    for "),
            "循环体抽取器疑似瞎了（一个循环都没抽到，而源码里有迭代写法）"
        );
        for body in bodies {
            assert_eq!(
                query_calls(&body),
                0,
                "⚠️ 循环体内出现 sqlx 查询调用 ⇒ N+1：\n{body}"
            );
        }
    }

    /// 三条 `t_customer` 只读查询，各一个函数、一条语句。
    ///
    /// ⚠️ 这个数字的作用是「新增第 4 条查询时**逼你**看一眼是不是又写了一条逐行查」，
    /// **不是**语句数契约 —— 契约是「与行数无关」，由上一条守。
    #[test]
    fn exactly_three_customer_queries_each_in_its_own_helper() {
        assert_eq!(
            query_calls(code()),
            2,
            "service.rs 里的查询调用点变了（预期 2：`customer_name` / \
             `customer_names`）。新增前请确认它不是逐行查客户名。"
        );
        for helper in ["customer_name", "customer_names"] {
            assert_eq!(
                query_calls(&fn_body(helper)),
                1,
                "`{helper}` 应恰好一条查询"
            );
        }
    }

    /// 列表端点的客户名**必须**走批量那条（`id = ANY($1)`），不许换成逐行查。
    #[test]
    fn list_templates_joins_customer_names_in_memory() {
        let body = fn_body("list_templates");
        assert!(
            body.contains("customer_names("),
            "`list_templates` 必须调用批量取名的 `customer_names`：\n{body}"
        );
        assert!(
            !body.contains("customer_name("),
            "`list_templates` 不许调单条取名的 `customer_name`（那是 N+1）：\n{body}"
        );
        assert!(
            body.contains("let ids: Vec<i64>") && body.contains("names.get("),
            "`list_templates` 应是「先收集 ids → 批量查 → 内存 join」三步：\n{body}"
        );
    }
}
