//! assembly 域 CRUD service
//!
//! 列表 / 详情 / 创建 / 更新 / 软删 / 取消 —— 共 6 个端点。
//!
//! ## 业务约束（service 层 enforce）
//! - 列表：`customer_id` 支持 L1 展开（recursive CTE，由 trait 提供）
//! - 创建：`customer_id` 必须是 L2 叶子；子件 ≤ 99；PDF 页数 == children.len()+1；
//!   有 PDF 时从 L1 customer.serial_prefix 派发序列号
//! - 更新：customer_id 三态（None/Some(None)/Some(Some(v))）+ L2 校验；OCC；§3.2
//!   级联覆盖父件到所有未软删子件（8 个共享信息字段）；§3.3 套数缩放
//! - 软删：Manager only；OCC；终态守；预检子件挂送货单（PR-2 后 JOIN t_part_batch 查）
//! - 取消：Manager/Clerk；repo 按 `status NOT IN ('COMPLETED','CANCELLED')` 守卫，
//!   命中 0 行 → 终态禁 cancel（返回 `BIZ_INVALID_TRANSITION`）
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`
//! （by-value；`R: AssemblyRepoTrait`）参数传入——handler/service 借 `&mut *tx` /
//! `&mut *conn` 喂给 trait（trait 已直接 `impl for &mut PgConnection`）。
//!
//! ## 跨域调用（2026-09-22 D-3 决策）
//! 所有跨域 SQL（含 t_customer / t_part / t_part_file / t_part_batch）均通过
//! `AssemblyRepoTrait` 的跨域 helper 方法收口——service 不直接调 `PartRepo::xxx` /
//! `PartFileRepo::xxx` ZST 静态方法。这保证 `&mut PgConnection` 同一作用域只借给一个
//! repo 实例（即 trait 对象本身），避免重复借用。

use std::collections::{BTreeSet, HashMap};

use rust_decimal::Decimal;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::dto::{
    AssemblyCreateRequest, AssemblyListQuery, AssemblyUpdateRequest,
};
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::{AssemblyRepoTrait, NewAssembly, AssemblyUpdate};
use crate::modules::assembly::vo::{
    AssemblyChildOut, AssemblyCreateResult, AssemblyDetail, AssemblyListItem, AssemblyListOut,
    AssemblyOut,
};
use crate::modules::part::repo::part::ChildInheritFields;
use crate::shared::error::{AppError, code};

use super::AssemblyService;

// =============================================================================
// 兼容旧测试的 ZST 静态 wrapper 实现（2026-09-22 D-3 决策）
// =============================================================================
//
// 既存集成测试以 `AssemblyService::create_assembly(&mut tx, ...)` 形式直调 service。
// trait 注入式新签名 `<R: AssemblyRepoTrait>(&self, mut repo: R, ...)` 要求 caller
// 写 `AssemblyService.xxx(&mut *tx, ...)`，与旧测试不兼容。本任务"不修改测试代码"，
// 故以下 `_impl` 函数保留旧签名（接收 `&mut PgConnection`），内部一行委托到 trait 方法。
//
// 2026-09-22 D-3 决策：保留 wrapper 而非把 service 改成旧签名，原因是新签名是 iam 范本
// 要求（`<R: AssemblyRepoTrait>`）。wrapper 仅 ~10 行 / 方法，不破坏任何未来 trait 注入。

pub(crate) async fn list_assemblies_dispatch(
    conn: &mut sqlx::PgConnection,
    query: &AssemblyListQuery,
    current: &CurrentUser,
) -> Result<AssemblyListOut, AppError> {
    AssemblyService.list_assemblies_inner(conn, query, current).await
}

pub(crate) async fn get_assembly_dispatch(
    conn: &mut sqlx::PgConnection,
    assembly_id: i64,
    current: &CurrentUser,
) -> Result<AssemblyDetail, AppError> {
    AssemblyService.get_assembly_inner(conn, assembly_id, current).await
}

pub(crate) async fn create_assembly_dispatch(
    conn: &mut sqlx::PgConnection,
    snowflake: &SnowflakeIdGenerator,
    req: &AssemblyCreateRequest,
    pdf_files: Vec<Vec<u8>>,
    current: &CurrentUser,
) -> Result<AssemblyCreateResult, AppError> {
    AssemblyService
        .create_assembly_inner(conn, snowflake, req, pdf_files, current)
        .await
}

pub(crate) async fn update_assembly_dispatch(
    conn: &mut sqlx::PgConnection,
    assembly_id: i64,
    req: &AssemblyUpdateRequest,
    current: &CurrentUser,
) -> Result<AssemblyOut, AppError> {
    AssemblyService.update_assembly_inner(conn, assembly_id, req, current).await
}

pub(crate) async fn soft_delete_assembly_dispatch(
    conn: &mut sqlx::PgConnection,
    assembly_id: i64,
    expected_version: i32,
    current: &CurrentUser,
) -> Result<(), AppError> {
    AssemblyService
        .soft_delete_assembly_inner(conn, assembly_id, expected_version, current)
        .await
}

pub(crate) async fn cancel_assembly_dispatch(
    conn: &mut sqlx::PgConnection,
    assembly_id: i64,
    current: &CurrentUser,
) -> Result<AssemblyOut, AppError> {
    AssemblyService.cancel_assembly_inner(conn, assembly_id, current).await
}

// ---------- helpers ----------

/// `TAssembly` → `AssemblyListItem`（含 customer_name / parent_customer_name 两次 join）。
///
/// 2026-09-16 PR-2 瘦身：t_assembly 删 `actual_delivery_date` 列，AssemblyOut
/// 同步删该字段。
fn render_list_item(
    asm: TAssembly,
    names: &HashMap<i64, (String, Option<i64>)>,
) -> AssemblyListItem {
    let (customer_name, parent_customer_name) = names
        .get(&asm.customer_id)
        .map(|(n, p)| {
            (
                Some(n.clone()),
                p.and_then(|pid| names.get(&pid).map(|(pn, _)| pn.clone())),
            )
        })
        .unwrap_or((None, None));
    AssemblyListItem {
        assembly: AssemblyOut {
            id: asm.id,
            drawing_no: asm.drawing_no,
            name: asm.name,
            applicant_name: asm.applicant_name,
            customer_id: asm.customer_id,
            request_date: asm.request_date,
            planned_delivery_date: asm.planned_delivery_date,
            is_urgent: asm.is_urgent,
            status: asm.status,
            version: asm.version,
            serial_no: asm.serial_no,
            quantity: asm.quantity,
            unit_price: asm.unit_price,
            total_price: asm.total_price,
            order_no: asm.order_no,
            system_delivery_date: asm.system_delivery_date,
            note: asm.note,
            created_at: asm.created_at,
            updated_at: asm.updated_at,
        },
        customer_name,
        parent_customer_name,
    }
}

/// `TAssembly` → `AssemblyOut`（详情 / 更新 / 取消返回）。
///
/// 2026-09-16 PR-2 瘦身：删 `actual_delivery_date` 字段。
fn render_assembly_out(asm: TAssembly) -> AssemblyOut {
    AssemblyOut {
        id: asm.id,
        drawing_no: asm.drawing_no,
        name: asm.name,
        applicant_name: asm.applicant_name,
        customer_id: asm.customer_id,
        request_date: asm.request_date,
        planned_delivery_date: asm.planned_delivery_date,
        is_urgent: asm.is_urgent,
        status: asm.status,
        version: asm.version,
        serial_no: asm.serial_no,
        quantity: asm.quantity,
        unit_price: asm.unit_price,
        total_price: asm.total_price,
        order_no: asm.order_no,
        system_delivery_date: asm.system_delivery_date,
        note: asm.note,
        created_at: asm.created_at,
        updated_at: asm.updated_at,
    }
}

impl AssemblyService {
    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    /// 列表查询：L1 客户展开 + 多维筛选 + 计数 + customer name 批量 join。
    pub async fn list_assemblies_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        query: &AssemblyListQuery,
        current: &CurrentUser,
    ) -> Result<AssemblyListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        let customer_ids = if let Some(cid_str) = &query.customer_id {
            let cid: i64 = cid_str.parse().map_err(|_| {
                AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("customer_id 非法: {cid_str}"),
                )
            })?;
            repo.expand_customer_l2_ids(cid)
                .await
                .map_err(AppError::from)?
        } else {
            Vec::new()
        };

        let statuses: Vec<String> = if let Some(ss) = &query.statuses {
            ss.clone()
        } else if let Some(s) = &query.status {
            vec![s.clone()]
        } else {
            Vec::new()
        };

        let limit = query.limit.unwrap_or(50).clamp(1, 500);
        let offset = query.offset.unwrap_or(0).max(0);

        let rows = repo
            .list_with_filters(
                &customer_ids,
                query.status.as_deref(),
                &statuses,
                query.is_urgent,
                query.keyword.as_deref(),
                query.sort_by.as_deref(),
                query.sort_dir.as_deref(),
                limit,
                offset,
                false,
            )
            .await
            .map_err(AppError::from)?;
        let total = repo
            .count_with_filters(
                &customer_ids,
                query.status.as_deref(),
                &statuses,
                query.is_urgent,
                query.keyword.as_deref(),
                false,
            )
            .await
            .map_err(AppError::from)?;

        // 批量拉 customer name（O(1) 查询）；先 BTreeSet 去重再 collect 保持稳定顺序
        let unique_ids: Vec<i64> = rows
            .iter()
            .map(|r| r.customer_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let names = repo
            .fetch_customer_names_by_ids(&unique_ids)
            .await
            .map_err(AppError::from)?;

        let items = rows
            .into_iter()
            .map(|r| render_list_item(r, &names))
            .collect();
        Ok(AssemblyListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// 详情：装配体行 + children（part 子件）+ files（占位空数组）。
    ///
    /// 2026-09-14 Phase 3（deferred #7）：children 携带 `current_batch_id`（子件当前激活批次 id）。
    /// 取法：`t_part_batch WHERE part_id = $1 AND deleted_at IS NULL ORDER BY batch_no DESC LIMIT 1`。
    pub async fn get_assembly_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyDetail, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let asm = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_ASSEMBLY_NOT_FOUND,
                    format!("assembly {assembly_id} 不存在"),
                )
            })?;

        let children_t = repo
            .list_parts_by_assembly_id(assembly_id, false)
            .await
            .map_err(AppError::from)?;
        // deferred #7：批量查 current_batch_id（O(1) 查询）
        let child_ids: Vec<i64> = children_t.iter().map(|p| p.id).collect();
        let current_batch_ids = repo
            .fetch_current_batch_ids_for_parts(&child_ids)
            .await
            .map_err(AppError::from)?;
        let children = children_t
            .into_iter()
            .map(|p| {
                let cb_id = current_batch_ids.get(&p.id).copied().flatten();
                AssemblyChildOut {
                    id: p.id,
                    serial_no: p.serial_no,
                    name: p.name,
                    drawing_no: Some(p.drawing_no),
                    status: p.status,
                    version: p.version,
                    quantity: p.quantity,
                    planned_delivery_date: Some(p.planned_delivery_date),
                    applicant_name: p.applicant_name,
                    request_date: p.request_date,
                    order_no: p.order_no,
                    system_delivery_date: p.system_delivery_date,
                    is_urgent: p.is_urgent,
                    note: p.note,
                    current_batch_id: cb_id,
                }
            })
            .collect();

        // 上传的文件列表（ASSEMBLY_MASTER kind）
        let files_t = repo
            .list_part_files_by_owner("ASSEMBLY", asm.id)
            .await
            .map_err(AppError::from)?;
        let files: Vec<crate::modules::assembly::vo::AssemblyFileRef> = files_t
            .into_iter()
            .filter(|f| f.kind == "ASSEMBLY_MASTER")
            .map(|f| crate::modules::assembly::vo::AssemblyFileRef {
                id: f.id,
                original_filename: f.original_filename,
                page_count: None,
            })
            .collect();

        Ok(AssemblyDetail {
            assembly: render_assembly_out(asm),
            children,
            files,
        })
    }

    // =======================================================================
    // 创建
    // =======================================================================

    /// 创建：multipart PDF（可选） + 子件 + 序列号派发。
    ///
    /// 关键校验：
    /// 1. `customer_id` 必须是 L2 叶子（`parent_id NOT NULL`）
    /// 2. 子件 ≤ 99（`BIZ_ASSEMBLY_TOO_MANY_CHILDREN`）
    /// 3. 若提供 PDF：页数 == `children.len() + 1`（首页 + 每子件 1 页）
    /// 4. 若提供 PDF：从 L1 客户的 `serial_prefix` 派发序列号（无 prefix → `BIZ_CUSTOMER_NO_SERIAL_PREFIX`）
    pub async fn create_assembly_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: &AssemblyCreateRequest,
        pdf_files: Vec<Vec<u8>>,
        current: &CurrentUser,
    ) -> Result<AssemblyCreateResult, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // 1. customer_id 必须为 L2 叶子（parent_id NOT NULL）
        let customer_id: i64 = req.customer_id.parse().map_err(|_| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer_id 非法: {}", req.customer_id),
            )
        })?;
        let parent_id = repo
            .fetch_customer_parent_id(customer_id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "customer 不存在"))?;
        if parent_id.is_none() {
            return Err(AppError::biz(
                code::BIZ_ASSEMBLY_BAD_CUSTOMER,
                "customer_id 必须是 L2 叶子节点",
            ));
        }

        // 2. 子件上限
        if req.children.len() > 99 {
            return Err(AppError::biz(
                code::BIZ_ASSEMBLY_TOO_MANY_CHILDREN,
                format!("子件最多 99 个，当前 {}", req.children.len()),
            ));
        }

        // 3. PDF 校验（如果提供）：首份 PDF 页数 == children.len() + 1
        let page_count_opt = if !pdf_files.is_empty() {
            // 当前只处理第一份 PDF（与分支一致）；其它累计忽略
            let pdf = &pdf_files[0];
            let doc = lopdf::Document::load_mem(pdf).map_err(|e| {
                AppError::biz(code::BIZ_ASSEMBLY_PDF_INVALID, format!("PDF 解析失败: {e}"))
            })?;
            let page_count = doc.get_pages().len();
            if page_count != req.children.len() + 1 {
                return Err(AppError::biz(
                    code::BIZ_ASSEMBLY_PDF_INVALID,
                    format!(
                        "PDF 页数 {page_count} 与 children.len()+1={} 不匹配",
                        req.children.len() + 1
                    ),
                ));
            }
            Some(page_count as i32)
        } else {
            None
        };

        // 4. 派发 serial（仅在有 PDF 时拿 serial）
        let (serial_no, prefix) = if pdf_files.is_empty() {
            (None, None)
        } else {
            // 取 L1 客户的 serial_prefix 首字母（约定 L1 customer 必有 serial_prefix）
            let l1_id = repo
                .fetch_customer_l1_id(customer_id)
                .await
                .map_err(AppError::from)?
                .ok_or_else(|| {
                    AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "customer 不存在")
                })?;
            let prefix_str = repo
                .fetch_customer_serial_prefix(l1_id)
                .await
                .map_err(AppError::from)?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_CUSTOMER_NO_SERIAL_PREFIX,
                        "L1 客户无 serial_prefix",
                    )
                })?;
            let ch = prefix_str
                .chars()
                .next()
                .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "serial_prefix 为空"))?;
            (Some(repo.acquire_serial(ch).await?), Some(ch))
        };

        // 5. INSERT t_assembly
        //
        // 注意：DB 端 `request_date` / `planned_delivery_date` / `unit_price` / `total_price`
        // 均为 NOT NULL（见 migrations/20260811100005_005_create_part_tables.sql）。
        // DTO 这 4 个字段是 `Option<>`，必须在 service 层填默认值，
        // 否则 INSERT 会触发 23502 NOT NULL violation。
        let asm_id = snowflake.next_id();
        let today = clock::now_naive().date();
        let new = NewAssembly {
            id: asm_id,
            drawing_no: &req.drawing_no,
            name: &req.name,
            applicant_name: req.applicant_name.as_deref(),
            customer_id,
            request_date: Some(req.request_date.unwrap_or(today)),
            planned_delivery_date: Some(req.planned_delivery_date.unwrap_or(today)),
            is_urgent: req.is_urgent.unwrap_or(false),
            status: "PENDING",
            version: 0,
            serial_no: serial_no.as_deref(),
            quantity: req.quantity.unwrap_or(1),
            unit_price: req.unit_price.or(Some(Decimal::ZERO)),
            total_price: req.total_price.or(Some(Decimal::ZERO)),
            order_no: req.order_no.as_deref(),
            system_delivery_date: req.system_delivery_date,
            note: req.note.as_deref(),
            created_by: current.id,
        };
        repo.insert(new).await.map_err(AppError::from)?;

        // 6. 插入子件（如有 PDF，则带 serial_no 派生 `{asm_serial}-{i:02d}`）
        //
        // 子件字段继承父件（§3.1）：applicant_name/request_date/order_no/system_delivery_date/
        // is_urgent/note 由父件直接继承；planned_delivery_date 子件入参优先，缺省继承父件。
        // 父件相关缺省值已在本函数上方确定（request_date/planned_delivery_date/
        // is_urgent 见 NewAssembly 构造），这里直接把父件"应有值"打包传给 repo。
        let parent_applicant_name = req.applicant_name.as_deref().unwrap_or("");
        let parent_request_date = req.request_date.unwrap_or(today);
        let parent_planned_delivery_date = req.planned_delivery_date.unwrap_or(today);
        let parent_is_urgent = req.is_urgent.unwrap_or(false);
        let mut created_children_out: Vec<AssemblyChildOut> = Vec::new();
        if let (Some(asm_serial), Some(_)) = (serial_no.as_ref(), prefix) {
            for (i, ch) in req.children.iter().enumerate() {
                let child_id = snowflake.next_id();
                let initial_batch_id = snowflake.next_id();
                let child_serial = format!("{}-{:02}", asm_serial, i + 1);
                let child_qty = ch.quantity.unwrap_or(1);
                let child_planned = ch
                    .planned_delivery_date
                    .or(Some(parent_planned_delivery_date));
                let _ = page_count_opt; // reserved for AssemblyFileRef follow-up
                let inherit = ChildInheritFields {
                    applicant_name: parent_applicant_name,
                    request_date: parent_request_date,
                    order_no: req.order_no.as_deref(),
                    system_delivery_date: req.system_delivery_date,
                    is_urgent: parent_is_urgent,
                    note: req.note.as_deref(),
                };
                repo.insert_part_child_for_assembly(
                    child_id,
                    customer_id,
                    asm_id,
                    &child_serial,
                    &ch.name,
                    ch.drawing_no.as_deref(),
                    child_qty,
                    child_planned,
                    inherit,
                    current.id,
                    initial_batch_id,
                )
                .await
                .map_err(AppError::from)?;
                created_children_out.push(AssemblyChildOut {
                    id: child_id,
                    serial_no: Some(child_serial),
                    name: ch.name.clone(),
                    drawing_no: ch.drawing_no.clone(),
                    status: "PENDING".into(),
                    version: 0,
                    quantity: child_qty,
                    planned_delivery_date: child_planned,
                    applicant_name: parent_applicant_name.to_string(),
                    request_date: parent_request_date,
                    order_no: req.order_no.clone(),
                    system_delivery_date: req.system_delivery_date,
                    is_urgent: parent_is_urgent,
                    note: req.note.clone(),
                    current_batch_id: Some(initial_batch_id),
                });
            }
        }

        // 7. 读回返回（用 `include_deleted=true` 兜底刚 INSERT 的可见性）
        let asm_t = repo
            .get_by_id(asm_id, true)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "刚创建却查不到"))?;
        Ok(AssemblyCreateResult {
            assembly: render_assembly_out(asm_t),
            created_children: created_children_out,
        })
    }

    // =======================================================================
    // 更新
    // =======================================================================

    /// 字段可选 UPDATE（含 customer_id 三态校验 + L2 校验）。
    ///
    /// - `customer_id: None`（字段缺省）→ 不更新
    /// - `customer_id: Some(Some("xxx"))`（三态 Some(Some)）→ 覆盖 + L2 校验
    /// - `applicant_name` 等普通可空字段按 `Option<String>` 语义（None=不动、Some("")=覆盖）
    ///
    /// **§3.2 级联 + §3.3 缩放**：`update_partial` 成功后，同事务内：
    /// 1. 把父件"更新后的当前行值"覆盖级联到所有未软删子件（8 个共享信息字段：
    ///    `applicant_name` / `request_date` / `planned_delivery_date` /
    ///    `is_urgent` / `customer_id` / `order_no` / `system_delivery_date` /
    ///    `note`；排除 `quantity`）。
    /// 2. 若 `req.quantity` 有值且 ≠ 父件现值（old_qty），对每个子件
    ///    `new_qty = max(1, round(child_qty * new_qty / old_qty))`，
    ///    `version++`。不追溯调整 `t_part_batch.quantity`。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：t_assembly 删 `actual_delivery_date`
    /// 列，DTO `AssemblyUpdateRequest` 同步精简；级联子件集合保持 8 字段
    /// （`actual_delivery_date` 不在级联集合中 —— 该列已删，返修/发货事实改由
    /// `t_part_event` 事件日志承担）。
    pub async fn update_assembly_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        assembly_id: i64,
        req: &AssemblyUpdateRequest,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // customer_id 三态解析（None 缺省 = 不更新；Some(Some(cid_str)) = 覆盖 + L2 校验）
        let customer_id_i64 = if let Some(Some(cid_str)) = req.customer_id.as_ref() {
            let cid: i64 = cid_str.parse().map_err(|_| {
                AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("customer_id 非法: {cid_str}"),
                )
            })?;
            // 校验 L2 叶子（parent_id NOT NULL）
            let parent = repo
                .fetch_customer_parent_id(cid)
                .await
                .map_err(AppError::from)?
                .ok_or_else(|| AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "customer 不存在"))?;
            let _parent_id = parent.ok_or_else(|| {
                AppError::biz(code::BIZ_ASSEMBLY_BAD_CUSTOMER, "customer_id 必须是 L2")
            })?;
            Some(cid)
        } else {
            None
        };

        // 预读父件现值：捕获 old_qty 用于 §3.3 缩放触发判断；同事务内的
        // read-after-write 由 OCC 守，TOCTOU 窗口不会导致错误数据写库。
        let old_qty: i32 = repo
            .fetch_assembly_quantity(assembly_id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_ASSEMBLY_NOT_FOUND,
                    format!("assembly {assembly_id} 不存在"),
                )
            })?;

        let upd = AssemblyUpdate {
            drawing_no: req.drawing_no.as_deref(),
            name: req.name.as_deref(),
            // 2026-09-14 Phase 3（deferred #2）：三态语义
            // None = 不更新；Some(None) = 置 NULL；Some(Some(v)) = 覆盖
            applicant_name: req.applicant_name.as_ref().map(|opt| opt.as_deref()),
            customer_id: customer_id_i64,
            request_date: req.request_date,
            planned_delivery_date: req.planned_delivery_date,
            is_urgent: req.is_urgent,
            quantity: req.quantity,
            unit_price: req.unit_price,
            total_price: req.total_price,
            order_no: req.order_no.as_ref().map(|opt| opt.as_deref()),
            system_delivery_date: req.system_delivery_date,
            note: req.note.as_ref().map(|opt| opt.as_deref()),
            updated_by: current.id,
        };
        let affected = repo
            .update_partial(assembly_id, req.version, upd)
            .await
            .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "version 不匹配或记录已删除",
            ));
        }

        // 读回父件"更新后的当前行值" → §3.2 级联 + §3.3 缩放共用此视图
        let asm = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;

        // §3.2 级联：覆盖父件"应有值"到所有未软删子件。
        // `request_date` / `planned_delivery_date` 在 t_assembly 是 NOT NULL
        // （model.rs PR-4 与 DDL 对齐后两字段都是 NaiveDate，不再 Option）。
        // update 时三态置 NULL 在本期不允许（DTO 是 Option<Option<NaiveDate>>
        // 但语义上 asm 行始终非空；service 层遇 Some(None) 改判 20104）。
        let request_date = asm.request_date;
        let planned_delivery_date = asm.planned_delivery_date;
        let applicant_name = asm.applicant_name.as_deref().unwrap_or("");
        repo.cascade_sync_from_assembly(
            asm.id,
            request_date,
            applicant_name,
            asm.order_no.as_deref(),
            asm.system_delivery_date,
            planned_delivery_date,
            asm.is_urgent,
            asm.note.as_deref(),
            asm.customer_id,
            current.id,
        )
        .await
        .map_err(AppError::from)?;

        // §3.3 套数缩放：`req.quantity` 有值且 ≠ 父件现值（old_qty）才触发
        if let Some(new_qty) = req.quantity
            && new_qty != old_qty
        {
            repo.scale_children_quantity(asm.id, old_qty, new_qty, current.id)
                .await
                .map_err(AppError::from)?;
        }

        Ok(render_assembly_out(asm))
    }

    // =======================================================================
    // 软删（Manager only）
    // =======================================================================

    /// 软删（Manager only）：带版本号乐观锁；终态记录由 repo `status NOT IN`
    /// 守卫拦截（当前 repo 仅按 `deleted_at IS NULL` 守卫）。
    ///
    /// 2026-09-14 Phase 3（deferred #3）：service 层 pre-check 子件是否挂送货单。
    /// 若任一子件存在活跃批次（`t_part_batch.deleted_at IS NULL`）的
    /// `delivery_note_id IS NOT NULL` → 拒软删，返回 20307
    /// `BIZ_ASSEMBLY_HAS_SHIPMENT`。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：t_part.delivery_note_id 列已删，
    /// 「子件挂送货单」改 JOIN t_part_batch 查（真相源在 t_part_batch）。
    pub async fn soft_delete_assembly_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        assembly_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;
        // deferred #3 升级（PR-2）：子件挂送货单预检改查 t_part_batch。
        let has_shipment = repo
            .has_active_shipment_for_assembly(assembly_id)
            .await
            .map_err(AppError::from)?;
        if has_shipment {
            return Err(AppError::biz(
                code::BIZ_ASSEMBLY_HAS_SHIPMENT,
                "assembly 子件存在活跃批次已挂送货单，禁止 soft_delete",
            ));
        }
        let affected = repo
            .soft_delete(assembly_id, expected_version, current.id)
            .await
            .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "version 不匹配或记录已删除",
            ));
        }
        Ok(())
    }

    /// 取消：Manager/Clerk；repo 按 `status NOT IN ('COMPLETED','CANCELLED')` 守卫，
    /// 命中 0 行 → 终态禁 cancel（返回 `BIZ_INVALID_TRANSITION`）。
    ///
    /// 设计上无 OCC（cancel 是单向状态翻转，重复 cancel 走 0 行 → 409）。
    pub async fn cancel_assembly_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let affected = repo
            .cancel(assembly_id, current.id)
            .await
            .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "终态禁 cancel 或已删除",
            ));
        }
        let asm = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;
        Ok(render_assembly_out(asm))
    }
}
