//! assembly 域业务逻辑
//!
//! 对应 Python myERP/service/assembly_service.py（及 _<d>_*.py helper）。
//!
//! 事务边界：本文件**不**创建事务，由 handler 在外层 `state.pool.begin()`。
//! 所有 service 方法首项 `conn: &mut PgConnection`，末尾不 commit。

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use rust_decimal::Decimal;
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock;
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::dto::{
    AssemblyChildOut, AssemblyCreateRequest, AssemblyCreateResult, AssemblyDetail,
    AssemblyListItem, AssemblyListOut, AssemblyListQuery, AssemblyOut, AssemblyUpdateRequest,
};
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::{AssemblyListFilters, AssemblyRepo, AssemblyUpdate, NewAssembly};
use crate::modules::assembly::statemachine::{
    compute_assembly_target, AssemblyStatus,
};
use crate::modules::part::repo::part::ChildInheritFields;
use crate::modules::part::repo::PartRepo;
use crate::modules::part_file::repo::{hash_bytes, NewPartFile, PartFileRepo};
use crate::shared::error::{code, AppError};
use crate::shared::serial as serial_helper;

// ---------- helpers ----------

/// 把 `customer_id` 展开为下游 `t_assembly.customer_id IN (...)` 的查询列表。
///
/// - 若 `customer_id` 是 L1（`parent_id IS NULL`）：递归取所有 L2 子节点 + 自身；
/// - 否则：仅返回 `[customer_id]`（L2 叶子直接当 in-list 传入，SQL 写法一致）。
async fn expand_customer_id_to_l2(
    conn: &mut PgConnection,
    customer_id: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    // 先看自身是不是 L1（parent_id IS NULL） → 是：收集自身 + 所有 L2 子节点；否：仅自身
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL")
            .bind(customer_id)
            .fetch_optional(&mut *conn)
            .await?;
    match row {
        Some((None,)) => {
            // L1：递归取所有 L2（用 recursive CTE 一次拿齐）
            let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
                "WITH RECURSIVE subtree AS (SELECT id FROM t_customer WHERE id = ",
            );
            qb.push_bind(customer_id);
            qb.push(
                " AND deleted_at IS NULL UNION ALL SELECT c.id FROM t_customer c \
                 INNER JOIN subtree s ON c.parent_id = s.id WHERE c.deleted_at IS NULL) \
                 SELECT id FROM subtree",
            );
            let ids: Vec<(i64,)> = qb.build_query_as().fetch_all(&mut *conn).await?;
            Ok(ids.into_iter().map(|(i,)| i).collect())
        }
        _ => Ok(vec![customer_id]),
    }
}

/// 从 `t_serial_counter` 派发下一个序列号（`prefix` 是单字符业务 PK）。
///
/// 2026-09-14 Phase 3（deferred #5）：迁移到 `crate::shared::serial::acquire`，
/// 保留本 wrapper 为薄 alias（service 调用点直接用 `serial::acquire`）。
#[inline]
#[allow(dead_code)]
async fn acquire_serial(
    conn: &mut PgConnection,
    prefix: char,
) -> Result<String, AppError> {
    serial_helper::acquire(conn, prefix).await
}

/// 批量拉 customer 的 `(name, parent_id)`。返回 `HashMap<id, (name, parent_id)>`。
///
/// 防 N+1：`AssemblyService::list_assemblies` 一次性拉齐所有出现过的 customer。
/// `ids` 为空时直接返回空 HashMap，避免构造 `IN ()` 空 SQL。
async fn fetch_customer_names(
    conn: &mut PgConnection,
    ids: &[i64],
) -> Result<HashMap<i64, (String, Option<i64>)>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT id, name, parent_id FROM t_customer WHERE deleted_at IS NULL AND id IN (");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(*id);
    }
    qb.push(")");
    let rows: Vec<(i64, String, Option<i64>)> = qb.build_query_as().fetch_all(&mut *conn).await?;
    Ok(rows.into_iter().map(|(i, n, p)| (i, (n, p))).collect())
}

/// `TAssembly` → `AssemblyListItem`（含 customer_name / parent_customer_name 两次 join）。
///
/// 2026-09-16 PR-2 瘦身：t_assembly 删 `actual_delivery_date` 列，AssemblyOut
/// 同步删该字段。
fn render_list_item(asm: TAssembly, names: &HashMap<i64, (String, Option<i64>)>) -> AssemblyListItem {
    let (customer_name, parent_customer_name) = names
        .get(&asm.customer_id)
        .map(|(n, p)| (Some(n.clone()), p.and_then(|pid| names.get(&pid).map(|(pn, _)| pn.clone()))))
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

// ---------- service struct + 6 methods ----------

/// assembly 域业务门面。handler 经 `state.pool.begin()` 开 tx 后透传 `&mut tx`。
pub struct AssemblyService;

impl AssemblyService {
    /// 列表查询：L1 客户展开 + 多维筛选 + 计数 + customer name 批量 join。
    pub async fn list_assemblies(
        conn: &mut PgConnection,
        query: &AssemblyListQuery,
        current: &CurrentUser,
    ) -> Result<AssemblyListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::CncProgrammer])?;

        let customer_ids = if let Some(cid_str) = &query.customer_id {
            let cid: i64 = cid_str.parse().map_err(|_| {
                AppError::biz(code::BIZ_INVALID_VALUE, format!("customer_id 非法: {cid_str}"))
            })?;
            expand_customer_id_to_l2(conn, cid).await.map_err(AppError::from)?
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

        let filters = AssemblyListFilters {
            customer_ids: &customer_ids,
            status: query.status.as_deref(),
            statuses: &statuses,
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            sort_by: query.sort_by.as_deref(),
            sort_dir: query.sort_dir.as_deref(),
            limit,
            offset,
            include_deleted: false,
        };

        let rows = AssemblyRepo::list_with_filters(&mut *conn, &filters)
            .await
            .map_err(AppError::from)?;
        let total = AssemblyRepo::count_with_filters(&mut *conn, &filters)
            .await
            .map_err(AppError::from)?;

        // 批量拉 customer name（O(1) 查询）；先 BTreeSet 去重再 collect 保持稳定顺序
        let unique_ids: Vec<i64> = rows.iter().map(|r| r.customer_id).collect::<BTreeSet<_>>()
            .into_iter().collect();
        let names = fetch_customer_names(conn, &unique_ids).await.map_err(AppError::from)?;

        let items = rows.into_iter().map(|r| render_list_item(r, &names)).collect();
        Ok(AssemblyListOut { items, total, limit, offset })
    }

    /// 详情：装配体行 + children（part 子件）+ files（占位空数组）。
    ///
    /// 2026-09-14 Phase 3（deferred #7）：children 携带 `current_batch_id`（子件当前激活批次 id）。
    /// 取法：`t_part_batch WHERE part_id = $1 AND deleted_at IS NULL ORDER BY batch_no DESC LIMIT 1`。
    pub async fn get_assembly(
        conn: &mut PgConnection,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyDetail, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::CncProgrammer])?;
        let asm = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, format!("assembly {assembly_id} 不存在")))?;

        let children_t = PartRepo::list_by_assembly_id(&mut *conn, assembly_id, false)
            .await
            .map_err(AppError::from)?;
        // deferred #7：批量查 current_batch_id（O(1) 查询）
        let child_ids: Vec<i64> = children_t.iter().map(|p| p.id).collect();
        let current_batch_ids = Self::fetch_current_batch_ids(conn, &child_ids).await?;
        let children = children_t.into_iter().map(|p| {
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
        }).collect();

        // 上传的文件列表（ASSEMBLY_MASTER kind）
        let files_t = PartFileRepo::list_by_owner(&mut *conn, "ASSEMBLY", asm.id)
            .await
            .map_err(AppError::from)?;
        let files: Vec<crate::modules::assembly::dto::AssemblyFileRef> = files_t
            .into_iter()
            .filter(|f| f.kind == "ASSEMBLY_MASTER")
            .map(|f| crate::modules::assembly::dto::AssemblyFileRef {
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

    /// 批量拉子件 current_batch_id（最近一条活跃 batch）。
    /// 返回 `HashMap<part_id, Option<batch_id>>`；`None` 值表示子件无活跃 batch。
    async fn fetch_current_batch_ids(
        conn: &mut PgConnection,
        part_ids: &[i64],
    ) -> Result<HashMap<i64, Option<i64>>, sqlx::Error> {
        let mut out: HashMap<i64, Option<i64>> = HashMap::new();
        if part_ids.is_empty() {
            return Ok(out);
        }
        // 用 DISTINCT ON 取每个 part_id 的最大 batch_no 行
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT DISTINCT ON (part_id) part_id, id \
             FROM t_part_batch \
             WHERE part_id = ANY($1) AND deleted_at IS NULL \
             ORDER BY part_id, batch_no DESC",
        )
        .bind(part_ids)
        .fetch_all(&mut *conn)
        .await?;
        for (pid, bid) in rows {
            out.insert(pid, Some(bid));
        }
        for pid in part_ids {
            out.entry(*pid).or_insert(None);
        }
        Ok(out)
    }

    /// 创建：multipart PDF（可选） + 子件 + 序列号派发。
    ///
    /// 关键校验：
    /// 1. `customer_id` 必须是 L2 叶子（`parent_id NOT NULL`）
    /// 2. 子件 ≤ 99（`BIZ_ASSEMBLY_TOO_MANY_CHILDREN`）
    /// 3. 若提供 PDF：页数 == `children.len() + 1`（首页 + 每子件 1 页）
    /// 4. 若提供 PDF：从 L1 客户的 `serial_prefix` 派发序列号（无 prefix → `BIZ_CUSTOMER_NO_SERIAL_PREFIX`）
    pub async fn create_assembly(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &AssemblyCreateRequest,
        pdf_files: Vec<Vec<u8>>,
        current: &CurrentUser,
    ) -> Result<AssemblyCreateResult, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // 1. customer_id 必须为 L2 叶子（parent_id NOT NULL）
        let customer_id: i64 = req.customer_id.parse().map_err(|_| {
            AppError::biz(code::BIZ_INVALID_VALUE, format!("customer_id 非法: {}", req.customer_id))
        })?;
        let parent_check: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(customer_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::from)?;
        let parent_id = parent_check
            .ok_or_else(|| AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "customer 不存在"))?
            .0;
        if parent_id.is_none() {
            return Err(AppError::biz(code::BIZ_ASSEMBLY_BAD_CUSTOMER,
                "customer_id 必须是 L2 叶子节点"));
        }

        // 2. 子件上限
        if req.children.len() > 99 {
            return Err(AppError::biz(code::BIZ_ASSEMBLY_TOO_MANY_CHILDREN,
                format!("子件最多 99 个，当前 {}", req.children.len())));
        }

        // 3. PDF 校验（如果提供）：首份 PDF 页数 == children.len() + 1
        let page_count_opt = if !pdf_files.is_empty() {
            // 当前只处理第一份 PDF（与分支一致）；其它累计忽略
            let pdf = &pdf_files[0];
            let doc = lopdf::Document::load_mem(pdf)
                .map_err(|e| AppError::biz(code::BIZ_ASSEMBLY_PDF_INVALID, format!("PDF 解析失败: {e}")))?;
            let page_count = doc.get_pages().len();
            if page_count != req.children.len() + 1 {
                return Err(AppError::biz(code::BIZ_ASSEMBLY_PDF_INVALID,
                    format!("PDF 页数 {page_count} 与 children.len()+1={} 不匹配", req.children.len() + 1)));
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
            let l1_id: i64 = sqlx::query_scalar(
                "SELECT COALESCE(parent_id, id) FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(customer_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(AppError::from)?;
            let prefix_str: Option<String> = sqlx::query_scalar(
                "SELECT serial_prefix FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(l1_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(AppError::from)?;
            let p = prefix_str
                .ok_or_else(|| AppError::biz(code::BIZ_CUSTOMER_NO_SERIAL_PREFIX, "L1 客户无 serial_prefix"))?;
            let ch = p.chars().next().ok_or_else(|| {
                AppError::biz(code::BIZ_INVALID_VALUE, "serial_prefix 为空")
            })?;
            (Some(serial_helper::acquire(conn, ch).await?), Some(ch))
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
        AssemblyRepo::insert(&mut *conn, new).await.map_err(AppError::from)?;

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
                let child_planned = ch.planned_delivery_date.or(Some(parent_planned_delivery_date));
                let _ = page_count_opt; // reserved for AssemblyFileRef follow-up
                let inherit = ChildInheritFields {
                    applicant_name: parent_applicant_name,
                    request_date: parent_request_date,
                    order_no: req.order_no.as_deref(),
                    system_delivery_date: req.system_delivery_date,
                    is_urgent: parent_is_urgent,
                    note: req.note.as_deref(),
                };
                PartRepo::insert_child_for_assembly(
                    conn,
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
        let asm_t = AssemblyRepo::get_by_id(&mut *conn, asm_id, true)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "刚创建却查不到"))?;
        Ok(AssemblyCreateResult {
            assembly: render_assembly_out(asm_t),
            created_children: created_children_out,
        })
    }

    /// 字段可选 UPDATE（含 customer_id 三态校验 + L2 校验）。
    ///
    /// - `customer_id: None`（字段缺省）→ 不更新
    /// - `customer_id: Some(Some("xxx"))`（三态 Some(Some)）→ 覆盖 + L2 校验
    /// - `applicant_name` 等普通可空字段按 `Option<String>` 语义（None=不动、Some("")=覆盖）
    ///
    /// **§3.2 级联 + §3.3 缩放**：`update_partial` 成功后，同事务内：
    /// 1. 把父件"更新后的当前行值"覆盖级联到所有未软删子件（7 个共享信息字段；
    ///    排除 `quantity`）。
    /// 2. 若 `req.quantity` 有值且 ≠ 父件现值（old_qty），对每个子件
    ///    `new_qty = max(1, round(child_qty * new_qty / old_qty))`，
    ///    `version++`。不追溯调整 `t_part_batch.quantity`。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：t_assembly 删 `actual_delivery_date`
    /// 列，DTO `AssemblyUpdateRequest` 同步精简；级联子件集合保持 7 字段。
    pub async fn update_assembly(
        conn: &mut PgConnection,
        assembly_id: i64,
        req: &AssemblyUpdateRequest,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // customer_id 三态解析（None 缺省 = 不更新；Some(Some(cid_str)) = 覆盖 + L2 校验）
        let customer_id_i64 = if let Some(Some(cid_str)) = req.customer_id.as_ref() {
            let cid: i64 = cid_str.parse().map_err(|_| {
                AppError::biz(code::BIZ_INVALID_VALUE, format!("customer_id 非法: {cid_str}"))
            })?;
            // 校验 L2 叶子（parent_id NOT NULL）
            let parent: Option<Option<i64>> = sqlx::query_scalar(
                "SELECT parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(cid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(AppError::from)?;
            let _parent_id = parent
                .ok_or_else(|| AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "customer 不存在"))?
                .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_BAD_CUSTOMER, "customer_id 必须是 L2"))?;
            Some(cid)
        } else {
            None
        };

        // 预读父件现值：捕获 old_qty 用于 §3.3 缩放触发判断；同事务内的
        // read-after-write 由 OCC 守，TOCTOU 窗口不会导致错误数据写库。
        let old_qty: i32 = sqlx::query_scalar(
            "SELECT quantity FROM t_assembly WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(assembly_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, format!("assembly {assembly_id} 不存在")))?;

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
        let affected = AssemblyRepo::update_partial(&mut *conn, assembly_id, req.version, upd)
            .await
            .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "version 不匹配或记录已删除"));
        }

        // 读回父件"更新后的当前行值" → §3.2 级联 + §3.3 缩放共用此视图
        let asm = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;

        // §3.2 级联：覆盖父件"应有值"到所有未软删子件。
        // `request_date` / `planned_delivery_date` 在 t_assembly 是 NOT NULL，
        // 由 service 层在 create 时填 today 兜底；update 时三态置 NULL 在本期
        // 不允许（DTO 是 Option<Option<NaiveDate>> 但语义上 asm 行始终非空）。
        let today = clock::now_naive().date();
        let request_date = asm.request_date.unwrap_or(today);
        let planned_delivery_date = asm.planned_delivery_date.unwrap_or(today);
        let applicant_name = asm.applicant_name.as_deref().unwrap_or("");
        PartRepo::cascade_sync_from_assembly(
            &mut *conn,
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
            PartRepo::scale_children_quantity(&mut *conn, asm.id, old_qty, new_qty, current.id)
                .await
                .map_err(AppError::from)?;
        }

        Ok(render_assembly_out(asm))
    }

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
    pub async fn soft_delete_assembly(
        conn: &mut PgConnection,
        assembly_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;
        // deferred #3 升级（PR-2）：子件挂送货单预检改查 t_part_batch。
        let has_shipment: Option<(i64,)> = sqlx::query_as(
            "SELECT 1::bigint \
             FROM t_part_batch pb \
             JOIN t_part p ON p.id = pb.part_id \
             WHERE p.assembly_id = $1 \
               AND p.deleted_at IS NULL \
               AND pb.deleted_at IS NULL \
               AND pb.delivery_note_id IS NOT NULL \
             LIMIT 1",
        )
        .bind(assembly_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::from)?;
        if has_shipment.is_some() {
            return Err(AppError::biz(
                code::BIZ_ASSEMBLY_HAS_SHIPMENT,
                "assembly 子件存在活跃批次已挂送货单，禁止 soft_delete",
            ));
        }
        let affected = AssemblyRepo::soft_delete(&mut *conn, assembly_id, expected_version, current.id)
            .await
            .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "version 不匹配或记录已删除"));
        }
        Ok(())
    }

    /// 取消：Manager/Clerk；repo 按 `status NOT IN ('COMPLETED','CANCELLED')` 守卫，
    /// 命中 0 行 → 终态禁 cancel（返回 `BIZ_INVALID_TRANSITION`）。
    ///
    /// 设计上无 OCC（cancel 是单向状态翻转，重复 cancel 走 0 行 → 409）。
    pub async fn cancel_assembly(
        conn: &mut PgConnection,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let affected = AssemblyRepo::cancel(&mut *conn, assembly_id, current.id)
            .await
            .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(code::BIZ_INVALID_TRANSITION, "终态禁 cancel 或已删除"));
        }
        let asm = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;
        Ok(render_assembly_out(asm))
    }

    /// `POST /assemblies/{id}/start`：PENDING → IN_PROCESS（状态机守卫）。
    ///
    /// 2026-09-14 Phase 3（deferred #4）：独立端点暴露装配体进入加工态。
    /// 权限：Manager / Clerk；状态机 `PENDING → IN_PROCESS` 校验在 service 层。
    pub async fn start_assembly(
        conn: &mut PgConnection,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let asm = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;
        let from = AssemblyStatus::from_str(&asm.status).ok_or_else(|| {
            AppError::biz(code::BIZ_INVALID_VALUE, format!("未知 assembly status: {}", asm.status))
        })?;
        if !from.can_transition_to(AssemblyStatus::IN_PROCESS) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("start 状态机禁止: {} → IN_PROCESS", from.as_str()),
            ));
        }
        let affected = AssemblyRepo::update_status_if_not_terminal(
            &mut *conn,
            assembly_id,
            asm.version,
            AssemblyStatus::IN_PROCESS.as_str(),
            current.id,
        )
        .await
        .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "version 不匹配或已终态"));
        }
        let fresh = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "start 后查不到"))?;
        Ok(render_assembly_out(fresh))
    }

    /// `POST /assemblies/{id}/files`：multipart PDF 上传（deferred #1）。
    ///
    /// 复用 part_file 域上传逻辑（SHA-256 CAS + COS PUT + t_part_file INSERT）。
    /// owner_kind='ASSEMBLY'，kind='ASSEMBLY_MASTER'；多 PDF 用多 part。
    ///
    /// 返回 AssemblyFileRef 列表（不含下载 URL，前端用 `GET /part-files/{id}/url`）。
    pub async fn upload_assembly_files(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        assembly_id: i64,
        files: Vec<(Vec<u8>, String, String)>, // (bytes, filename, content_type)
        current: &CurrentUser,
    ) -> Result<Vec<crate::modules::assembly::dto::AssemblyFileRef>, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        // 校验 assembly 存在
        let asm = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;

        let mut out = Vec::with_capacity(files.len());
        for (bytes, filename, content_type) in files {
            // 扩展名校验（仅允许 PDF）
            let ext = crate::modules::part_file::policy::ext_of(&filename)
                .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "文件缺少扩展名"))?;
            if ext != "pdf" {
                return Err(AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!("ASSEMBLY_MASTER 仅接受 PDF，扩展名 {ext:?} 不允许"),
                ));
            }
            // SHA-256 → CAS
            let sha = hash_bytes(&bytes);
            if let Some(existing) = PartFileRepo::get_by_owner_kind_sha(&mut *conn, asm.id, "ASSEMBLY_MASTER", &sha).await? {
                out.push(crate::modules::assembly::dto::AssemblyFileRef {
                    id: existing.id,
                    original_filename: existing.original_filename,
                    page_count: None, // PDF 页数由 GET 时 lopdf 重算；这里省略
                });
                continue;
            }
            // 上传 COS
            let safe_filename = sanitize_cos_filename(&filename);
            let object_key = format!(
                "assembly/{}/ASSEMBLY_MASTER/{}_{}",
                asm.id,
                &sha[..16],
                safe_filename,
            );
            cos.put_object(&object_key, bytes.clone(), &content_type).await?;
            // INSERT
            let file_id = snowflake.next_id();
            let nf = NewPartFile {
                id: file_id,
                part_id: asm.id,
                owner_kind: "ASSEMBLY",
                kind: "ASSEMBLY_MASTER",
                file_type: "PDF",
                object_key: &object_key,
                original_filename: &filename,
                file_size: bytes.len() as i64,
                content_type: &content_type,
                upload_status: "READY",
                content_sha256: Some(&sha),
                created_by: current.id,
            };
            PartFileRepo::create_part_file(&mut *conn, nf).await?;
            out.push(crate::modules::assembly::dto::AssemblyFileRef {
                id: file_id,
                original_filename: filename,
                page_count: None,
            });
        }
        Ok(out)
    }
}

// ---------- sync hook (assembly-status-auto-sync Task 1) ----------

/// Sync hook result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// 父装配件已为终态 / 无子件 / 目标 == 当前 → 不写库
    NoChange,
    /// 实际更新了 t_assembly.status；handler 据此发 ASSEMBLY_UPDATED
    Changed(i64),
}

impl AssemblyService {
    /// 从单个 part 的状态变更回流到父装配件（同事务调用）。
    /// 1. 反查 `part.assembly_id`（None → NoChange）
    /// 2. 父已是 COMPLETED/CANCELLED → NoChange（Python 短路 L92）
    /// 3. 拉子件 status → `compute_assembly_target` → Some(target)
    /// 4. 取父当前 version + status；target == 当前 → NoChange
    /// 5. `update_status_if_not_terminal`；0 行 → VERSION_CONFLICT（事务回滚）
    /// 6. 返回 `Changed(assembly_id)`
    pub async fn sync_from_part_change(
        conn: &mut PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<SyncOutcome, AppError> {
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT assembly_id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(AppError::from)?;
        let Some((Some(assembly_id),)) = row else {
            return Ok(SyncOutcome::NoChange);
        };
        Self::sync_assembly_status(conn, assembly_id, current).await
    }

    /// 批量版本：传入本次批量成功的 part_id 列表；
    /// 用单条 SQL `SELECT DISTINCT assembly_id` 去重，再逐个 sync。
    pub async fn sync_from_part_changes(
        conn: &mut PgConnection,
        part_ids: &[i64],
        current: &CurrentUser,
    ) -> Result<Vec<SyncOutcome>, AppError> {
        if part_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Option<i64>,)> = sqlx::query_as(
            "SELECT DISTINCT assembly_id FROM t_part \
             WHERE id = ANY($1) AND assembly_id IS NOT NULL \
               AND deleted_at IS NULL",
        )
        .bind(part_ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(AppError::from)?;
        let assembly_ids: Vec<i64> = rows.into_iter().filter_map(|(a,)| a).collect();
        let mut out = Vec::with_capacity(assembly_ids.len());
        for aid in assembly_ids {
            out.push(Self::sync_assembly_status(conn, aid, current).await?);
        }
        Ok(out)
    }

    /// 实际聚合 + 翻转的核心；`sync_from_part_change` / `sync_from_part_changes` 共用。
    async fn sync_assembly_status(
        conn: &mut PgConnection,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<SyncOutcome, AppError> {
        // 父存在性 + 终态短路
        let asm = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, format!("assembly {assembly_id} 不存在"))
            })?;
        let current_status = AssemblyStatus::from_str(&asm.status).ok_or_else(|| {
            AppError::biz(code::BIZ_INVALID_VALUE, format!("未知 assembly status: {}", asm.status))
        })?;
        if matches!(current_status, AssemblyStatus::COMPLETED | AssemblyStatus::CANCELLED) {
            return Ok(SyncOutcome::NoChange);
        }

        // 聚合子件
        let children_statuses =
            AssemblyRepo::aggregate_children_status(&mut *conn, assembly_id)
                .await
                .map_err(AppError::from)?;
        let Some(target) = compute_assembly_target(children_statuses.iter().map(|s| s.as_str())) else {
            return Ok(SyncOutcome::NoChange);
        };

        // target == current → NoChange
        if target == current_status {
            return Ok(SyncOutcome::NoChange);
        }

        // OCC 翻转
        let affected = AssemblyRepo::update_status_if_not_terminal(
            &mut *conn,
            assembly_id,
            asm.version,
            target.as_str(),
            current.id,
        )
        .await
        .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("assembly {assembly_id} version {} 已变化或已终态", asm.version),
            ));
        }
        Ok(SyncOutcome::Changed(assembly_id))
    }
}

// ---------- helpers for upload-files (deferred #1) ----------

/// 把 client-supplied filename 清洗为 COS object key 安全字符串：
/// - 保留 ASCII 字母 / 数字 / `.` / `-` / `_`
/// - 其它字符（含中文 / 空格）替换为 `_`
/// - 长度上限 80 字符
fn sanitize_cos_filename(name: &str) -> String {
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

#[cfg(test)]
mod upload_helpers_tests {
    use super::*;

    #[test]
    fn sanitize_cos_filename_basic() {
        assert_eq!(sanitize_cos_filename("master.pdf"), "master.pdf");
        assert_eq!(sanitize_cos_filename("图纸 v2.pdf"), "___v2.pdf");
    }

    #[test]
    fn sanitize_cos_filename_empty_fallback() {
        assert_eq!(sanitize_cos_filename(""), "file");
        // 中文 → "__"（替换为下划线后非空，保留而非 fallback）
        assert_eq!(sanitize_cos_filename("中文"), "__");
    }
}