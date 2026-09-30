//! com::union_list 域单端点业务逻辑（2026-09-29 新增）
//!
//! ## 范围
//! - `UnionListService::list_union_items(query, current)` —— 跨表合并视图端点
//!   `GET /api/v2/com/union-list` 的核心实现。row_type 三态（ALL / PART /
//!   ASSEMBLY）由 DTO 层 `UnionListQuery` 承载 → `RowType` enum normalize →
//!   service 端 dispatch。
//!
//! ## 设计要点（plan §3-4）
//! - **ALL 模式 SQL UNION ALL + pushdown**：每段 SQL 内部 `LIMIT (offset+limit)`
//!   OFFSET 0，外层 UNION 后再做 ORDER BY + LIMIT + OFFSET（修分页 bug：
//!   原 ALL 模式 `segment_limit.clamp(1,200)` 在 deep offset 时返回空集）。
//! - **PART 模式直走 `PartRepo::list_with_filters(part_only=true)`** + count；
//!   单段无需 pushdown。
//! - **ASSEMBLY 模式直走 `AssemblyRepo::list_with_filters`** + count；
//!   t_assembly 无 batch 派生字段（locations / holder_ids 忽略）。
//! - **enrichment 分桶**：PART 行 `location` / `holder_name`；ASSEMBLY 行
//!   `child_count` / `has_children`；所有行 `customer_name` / `l1_customer_name`。
//!
//! ## 与 part::service::crud 边界
//! - 原 `PartService::list_parts` 的 ALL / ASSEMBLY 分支（`list_parts_assembly_only` /
//!   `list_parts_all_merged` / `project_assembly_to_part_list_item` / `fetch_child_counts` /
//!   `parse_list_filters_for_assembly`）已下沉到本域（plan §5）。
//! - `parse_list_filters` 留在 part 域：本端点复用其等价的内联版本（字段解析 +
//!   `expand_customer_id`），避免跨域 pub 暴露内部 helper。
//!
//! ## 2026-09-30 新增：`planned_delivery_date_from/to` 日期窗口过滤
//! 修前端 dashboard UpcomingDeliveryListDrawer 的隐藏 bug —— 前端已传这俩参数
//! 但本 DTO 之前没有对应字段，参数被静默丢弃。本 service 层在 `parse_filters`
//! 解析 `YYYY-MM-DD` → `chrono::NaiveDate`，非法格式 → 40001 VALIDATION_ERROR。
//! PART / ALL / ASSEMBLY 三模式全部生效。
//!
//! ## SQL 引用
//! - PART 段：`part/repo/sql/part_sql.rs::list_with_filters`（part_only=true）
//! - ASSEMBLY 段：`assembly/repo/sql.rs::list_with_filters`
//! - ALL 段：本域 `repo/sql.rs::list_union_all_with_filters`

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::sql::{AssemblyListFilters, AssemblyRepo};
use crate::modules::part::repo::PartListFilters;
use crate::modules::part::repo::PartRepo;
use crate::modules::part::service::list_enrichment::enrich_part_list_with_location_and_holder;
use crate::modules::part::vo::PartListItem;
use crate::shared::error::AppError;

use super::super::dto::{RowType, UnionListQuery};
use super::super::repo::{UnionListRepo, UnionListRow};
use super::super::vo::PartListOut;

/// 跨表合并视图端点业务逻辑。
///
/// 行为：
/// 1. normalize `row_type`（`None` / `"ALL"` / `""` → `All`；`"PART"` / `"ASSEMBLY"` 合法；其它 → 40001）。
/// 2. 解析共享筛选条件（`expand_customer_id` + 字符串切分）。
/// 3. 按 `RowType` dispatch：
///    - `Part` → `PartRepo::list_with_filters` + `count_with_filters`
///    - `Assembly` → `AssemblyRepo::list_with_filters` + `count_with_filters`
///    - `All` → `UnionListRepo::list_union_all_with_filters`（UNION ALL +
///      pushdown）+ 两次 `count_with_filters`
/// 4. enrichment 分桶（`customer_name` / `l1_customer_name` 全部行；
///    `location` / `holder_name` 仅 PART 行；`child_count` / `has_children`
///    仅 ASSEMBLY 行）。
/// 5. 投影为 `PartListItem`（已含 `row_type` 字段）→ 序列化为 `PartListOut`。
///
/// 权限：4 角色全开放（Manager / Clerk / Inspector / CncProgrammer），与
/// `part/handler/crud.rs::LIST_PART_ROLES` 一致。
pub struct UnionListService;

impl UnionListService {
    pub async fn list_union_items(
        conn: &mut PgConnection,
        query: &UnionListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        let row_type = RowType::parse(query.row_type.as_deref())?;

        // limit / offset 与 PART 域 PART 模式对齐（clamp 1..=200；offset >= 0）
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);

        match row_type {
            RowType::Part => Self::list_part(conn, query, limit, offset, current).await,
            RowType::Assembly => Self::list_assembly(conn, query, limit, offset, current).await,
            RowType::All => Self::list_all(conn, query, limit, offset, current).await,
        }
    }

    // ===== PART 单段模式 =====

    /// PART 单段：直走 `PartRepo::list_with_filters(part_only=true)` + count。
    async fn list_part(
        mut conn: &mut PgConnection,
        query: &UnionListQuery,
        limit: i64,
        offset: i64,
        _current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        let parsed = parse_filters(&mut *conn, query).await?;

        let filters = PartListFilters {
            customer_ids: &parsed.customer_ids,
            status: query.status.as_deref(),
            statuses: &parsed.statuses,
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            locations: &parsed.locations,
            holder_ids: &parsed.holder_ids,
            // 2026-09-30 新增：日期窗口过滤（与 SQL WHERE `>=` / `<=` 一致）。
            planned_delivery_date_from: parsed.planned_delivery_date_from,
            planned_delivery_date_to: parsed.planned_delivery_date_to,
            part_only: true, // PART-only 模式强制打开装配体子件守卫
            sort_by: &parsed.sort_by,
            sort_dir: &parsed.sort_dir,
            limit,
            offset,
            include_deleted: false,
        };
        let rows = PartRepo::list_with_filters(&mut *conn, &filters).await?;
        let total = PartRepo::count_with_filters(&mut *conn, &filters).await?;

        let part_ids: Vec<i64> = rows.iter().map(|p| p.id).collect();
        // `&mut PgConnection` 直接实现 `PartRepoTrait`（见
        // `part/repo/mod.rs::impl PartRepoTrait for &mut PgConnection`），故
        // `&mut *conn` 喂给 `enrich_part_list_with_location_and_holder` 即可
        // （函数形参 `repo: &mut R` 推断 `R = PgConnection` 因 reborrow，但
        // `PgConnection` 也实现了 PartRepoTrait via... no, only `&mut PgConnection` does.
        // 因此用 `&mut *conn` reborrow 一次：`&mut (&mut PgConnection)`，R = &mut PgConnection）。
        let batch_enrichment =
            enrich_part_list_with_location_and_holder(&mut conn, &part_ids).await?;

        let mut items = Vec::with_capacity(rows.len());
        for p in rows {
            let (cn, l1cn) = lookup_customer_names(&mut *conn, p.customer_id).await?;
            let (loc, holder) = batch_enrichment.get(&p.id).cloned().unwrap_or((None, None));
            let mut item: PartListItem = p.into();
            item.customer_name = cn;
            item.l1_customer_name = l1cn;
            item.location = loc;
            item.holder_name = holder;
            items.push(item);
        }
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    // ===== ASSEMBLY 单段模式 =====

    /// ASSEMBLY 单段：直走 `AssemblyRepo::list_with_filters` + count。
    async fn list_assembly(
        conn: &mut PgConnection,
        query: &UnionListQuery,
        limit: i64,
        offset: i64,
        _current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        let parsed = parse_filters(&mut *conn, query).await?;

        let filters = AssemblyListFilters {
            customer_ids: &parsed.customer_ids,
            status: query.status.as_deref(),
            statuses: &parsed.statuses, // 与 assembly 域 list_assemblies 同语义
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            // 2026-09-30 新增：日期窗口过滤（与 SQL WHERE `>=` / `<=` 一致）。
            planned_delivery_date_from: parsed.planned_delivery_date_from,
            planned_delivery_date_to: parsed.planned_delivery_date_to,
            sort_by: Some(&parsed.sort_by),
            sort_dir: Some(&parsed.sort_dir),
            limit,
            offset,
            include_deleted: false,
        };
        let asm_rows: Vec<TAssembly> =
            AssemblyRepo::list_with_filters(&mut *conn, &filters).await?;
        let total = AssemblyRepo::count_with_filters(&mut *conn, &filters).await?;

        let asm_ids: Vec<i64> = asm_rows.iter().map(|a| a.id).collect();
        let child_counts = fetch_child_counts(&mut *conn, &asm_ids).await?;

        let mut items = Vec::with_capacity(asm_rows.len());
        for a in asm_rows {
            let (cn, l1cn) = lookup_customer_names(&mut *conn, a.customer_id).await?;
            let cc = child_counts.get(&a.id).copied().unwrap_or(0);
            let mut item: PartListItem = project_assembly_to_part_list_item(a);
            item.customer_name = cn;
            item.l1_customer_name = l1cn;
            item.child_count = Some(cc);
            item.has_children = cc > 0;
            items.push(item);
        }
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    // ===== ALL 模式：UNION ALL + pushdown =====

    /// ALL 模式：单段 pushdown `offset + limit` + UNION ALL + 外层 ORDER BY +
    /// LIMIT + OFFSET（plan §3）。
    ///
    /// count 策略：`part_total + asm_total`（两次 `count_with_filters`），不查
    /// union 表（union 计数代价高）。
    async fn list_all(
        mut conn: &mut PgConnection,
        query: &UnionListQuery,
        limit: i64,
        offset: i64,
        _current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        let parsed = parse_filters(&mut *conn, query).await?;

        // 2026-09-29 修复：移除原 segment_limit.clamp(1,200) 硬截——deep offset (>=200)
        // 时 part_seg / asm_seg 各只返前 200 行，UNION 表最多 400 行，外层 OFFSET
        // 必然返空（与原 list_parts_all_merged bug 完全同形）。
        // 现在按 plan §3 直接用 offset+limit：每段保证取够全局排序 [offset,
        // offset+limit) 区间所需的行。offset/limit 来自入参，已在外层校验（见
        // 上方 `limit = query.limit.unwrap_or(50).clamp(1, 200)` /
        // `offset = query.offset.unwrap_or(0).max(0)`）。
        //
        // 保守上限：避免 caller 误传巨大 offset 时 UNION 表膨胀（每段最多 1 万行）。
        let pushdown_limit = (limit + offset).min(10_000);

        // ----- ALL 段 UNION ALL + pushdown -----
        let rows = UnionListRepo::list_union_all_with_filters(
            &mut *conn,
            &parsed.customer_ids,
            query.status.as_deref(),
            &parsed.statuses,
            query.is_urgent,
            query.keyword.as_deref(),
            &parsed.locations,
            &parsed.holder_ids,
            &parsed.sort_by,
            &parsed.sort_dir,
            pushdown_limit,
            limit,
            offset,
            // 2026-09-30 新增：日期窗口过滤下推到 part_seg / asm_seg 段 WHERE。
            parsed.planned_delivery_date_from,
            parsed.planned_delivery_date_to,
        )
        .await?;

        // ----- 两次 count（PART + ASSEMBLY）-----
        let part_filters = PartListFilters {
            customer_ids: &parsed.customer_ids,
            status: query.status.as_deref(),
            statuses: &parsed.statuses,
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            locations: &parsed.locations,
            holder_ids: &parsed.holder_ids,
            // 2026-09-30 新增：日期窗口过滤。
            planned_delivery_date_from: parsed.planned_delivery_date_from,
            planned_delivery_date_to: parsed.planned_delivery_date_to,
            part_only: true, // part 段排除装配体子件
            sort_by: &parsed.sort_by,
            sort_dir: &parsed.sort_dir,
            limit,
            offset,
            include_deleted: false,
        };
        let part_total = PartRepo::count_with_filters(&mut *conn, &part_filters).await?;
        let asm_total = AssemblyRepo::count_with_filters(
            &mut *conn,
            &AssemblyListFilters {
                customer_ids: &parsed.customer_ids,
                status: query.status.as_deref(),
                statuses: &parsed.statuses,
                is_urgent: query.is_urgent,
                keyword: query.keyword.as_deref(),
                // 2026-09-30 新增：日期窗口过滤。
                planned_delivery_date_from: parsed.planned_delivery_date_from,
                planned_delivery_date_to: parsed.planned_delivery_date_to,
                sort_by: Some(&parsed.sort_by),
                sort_dir: Some(&parsed.sort_dir),
                limit: 0,
                offset: 0,
                include_deleted: false,
            },
        )
        .await?;

        // ----- enrichment 分桶 -----
        // PART 行：`location` / `holder_name`（按 row_type 过滤后批量派
        // 生）；ASSEMBLY 行：`child_count` / `has_children`（一次性 GROUP BY）。
        // 所有行：`customer_name` / `l1_customer_name`。
        let part_ids: Vec<i64> = rows
            .iter()
            .filter(|r| r.row_type == "PART")
            .map(|r| r.id)
            .collect();
        let asm_ids: Vec<i64> = rows
            .iter()
            .filter(|r| r.row_type == "ASSEMBLY")
            .map(|r| r.id)
            .collect();
        let batch_enrichment =
            enrich_part_list_with_location_and_holder(&mut conn, &part_ids).await?;
        let child_counts = fetch_child_counts(&mut *conn, &asm_ids).await?;

        let mut items = Vec::with_capacity(rows.len());
        for r in rows {
            let (cn, l1cn) = lookup_customer_names(&mut *conn, r.customer_id).await?;
            let mut item = union_row_to_part_list_item(r);
            item.customer_name = cn;
            item.l1_customer_name = l1cn;
            if item.row_type.as_deref() == Some("PART") {
                let (loc, holder) = batch_enrichment
                    .get(&item.id)
                    .cloned()
                    .unwrap_or((None, None));
                item.location = loc;
                item.holder_name = holder;
            } else if item.row_type.as_deref() == Some("ASSEMBLY") {
                let cc = child_counts.get(&item.id).copied().unwrap_or(0);
                item.child_count = Some(cc);
                item.has_children = cc > 0;
            }
            items.push(item);
        }
        Ok(PartListOut {
            items,
            total: part_total + asm_total,
            limit,
            offset,
        })
    }
}

// ===========================================================================
//  Helpers（pub(super) 仅本域可见）
// ===========================================================================

/// `UnionListQuery` 解析结果。全部 String / Vec 在 helper 内持有，借给下游
/// `PartListFilters` / `AssemblyListFilters` 时取 ref。
///
/// 2026-09-30 新增：`planned_delivery_date_from/to` —— `YYYY-MM-DD` 解析后的
/// `NaiveDate`（已校验），None 表示该端不参与 SQL 过滤。
struct ParsedFilters {
    sort_by: String,
    sort_dir: String,
    customer_ids: Vec<i64>,
    statuses: Vec<String>,
    locations: Vec<String>,
    holder_ids: Vec<i64>,
    planned_delivery_date_from: Option<NaiveDate>,
    planned_delivery_date_to: Option<NaiveDate>,
}

/// 解析 `UnionListQuery` 为内部变量（`parse_list_filters` 在 part 域的镜像）。
///
/// 注：本域不复用 `part::service::crud::parse_list_filters`，原因：
/// - 该 helper 是 part 域私有，不 `pub(super)`；
/// - 字段集已稳定一致，直接复制实现是 0 成本 + 0 跨域依赖。
async fn parse_filters(
    conn: &mut PgConnection,
    query: &UnionListQuery,
) -> Result<ParsedFilters, AppError> {
    // 排序键白名单：7 键（去掉 SERIAL_NO，因 t_assembly 上无对应列）。
    let sort_by = [
        "CREATED_AT",
        "UPDATED_AT",
        "PLANNED_DELIVERY_DATE",
        "REQUEST_DATE",
        "DRAWING_NO",
        "NAME",
        "SYSTEM_DELIVERY_DATE", // 2026-09-30 新增（dashboard「最紧急工单」按系统交期排序）
    ]
    .iter()
    .find(|&&s| Some(s) == query.sort_by.as_deref())
    .copied()
    .unwrap_or("CREATED_AT")
    .to_string();
    let sort_dir = if query
        .sort_dir
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("ASC"))
        .unwrap_or(false)
    {
        "ASC"
    } else {
        "DESC"
    }
    .to_string();

    let customer_ids: Vec<i64> = if let Some(cid) = query.customer_id {
        expand_customer_id(conn, cid).await?
    } else {
        Vec::new()
    };
    let statuses: Vec<String> = query
        .statuses
        .as_deref()
        .map(|s| {
            s.split(',')
                .filter(|x| !x.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    let locations: Vec<String> = query
        .locations
        .as_deref()
        .map(|s| {
            s.split(',')
                .filter(|x| !x.is_empty())
                .map(|s| s.trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    let holder_ids: Vec<i64> = match query.holder_ids.as_deref() {
        Some(s) if !s.is_empty() => s
            .split(',')
            .filter(|x| !x.is_empty())
            .map(|x| {
                x.trim()
                    .parse::<i64>()
                    .map_err(|_| AppError::validation(format!("holder_ids 含非法雪花 ID: {x}")))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => Vec::new(),
    };
    // 2026-09-30 新增：日期窗口解析（`YYYY-MM-DD`）。非法格式 → 40001 VALIDATION_ERROR。
    // 任一端缺失 → 对应 None（与日期窗口 `<` / `>` NULL 短路语义一致）。
    let planned_delivery_date_from = parse_optional_date(
        query.planned_delivery_date_from.as_deref(),
        "planned_delivery_date_from",
    )?;
    let planned_delivery_date_to = parse_optional_date(
        query.planned_delivery_date_to.as_deref(),
        "planned_delivery_date_to",
    )?;
    Ok(ParsedFilters {
        sort_by,
        sort_dir,
        customer_ids,
        statuses,
        locations,
        holder_ids,
        planned_delivery_date_from,
        planned_delivery_date_to,
    })
}

/// 解析可选日期 query 字段为 `NaiveDate`（2026-09-30 新增）。
///
/// - `None` / `Some("")` → `Ok(None)`（不参与过滤）
/// - `Some(s)` → `NaiveDate::parse_from_str(s, "%Y-%m-%d")`，失败 → `40001`
///   VALIDATION_ERROR，带字段名 + 原值便于前端定位。
fn parse_optional_date(
    raw: Option<&str>,
    field_name: &'static str,
) -> Result<Option<NaiveDate>, AppError> {
    match raw {
        Some(s) if !s.trim().is_empty() => NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
            .map(Some)
            .map_err(|e| {
                AppError::validation(format!("{field_name} 非法: {s}（必须是 YYYY-MM-DD: {e}）"))
            }),
        _ => Ok(None),
    }
}

/// 展开 `customer_id` 为 `[id]`（含自身 + 子节点）。
///
/// 语义与 `part::service::crud::expand_customer_id` 完全一致（从原 part 域
/// 服务层下沉到 com 域）：L1 → [自身 + 全部 L2 子节点]；L2 → [自身 + 同 L1
/// 下所有兄弟 L2 ids]。
async fn expand_customer_id(conn: &mut PgConnection, cid: i64) -> Result<Vec<i64>, AppError> {
    use crate::shared::error::code;
    let row: Option<(i64, Option<i64>)> =
        sqlx::query_as("SELECT id, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL")
            .bind(cid)
            .fetch_optional(&mut *conn)
            .await?;
    let (_id, parent_id) = row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("customer {cid} 不存在"),
        )
    })?;
    if let Some(p) = parent_id {
        let mut rows: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE parent_id = $1 AND deleted_at IS NULL",
        )
        .bind(p)
        .fetch_all(&mut *conn)
        .await?;
        if !rows.contains(&cid) {
            rows.push(cid);
        }
        Ok(rows)
    } else {
        sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE (parent_id = $1 OR id = $1) AND deleted_at IS NULL",
        )
        .bind(cid)
        .fetch_all(&mut *conn)
        .await
        .map_err(Into::into)
    }
}

/// 取客户名 + L1 名（`part::service::crud::lookup_customer_names` 同形私有版本）。
async fn lookup_customer_names(
    conn: &mut PgConnection,
    customer_id: i64,
) -> Result<(Option<String>, Option<String>), AppError> {
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT name, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (name, parent_id) = match row {
        Some(r) => r,
        None => return Ok((None, None)),
    };
    let l1_name = match parent_id {
        Some(pid) => {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(pid)
            .fetch_optional(&mut *conn)
            .await?
        }
        None => Some(name.clone()),
    };
    Ok((Some(name), l1_name))
}

/// 一次性 GROUP BY 拿一组 assembly_id 的子件计数（从 `part::service::crud` 下沉）。
///
/// 空 ids → 返回空 HashMap（不发起 SQL）。PART / ALL 段装配件行专用。
async fn fetch_child_counts(
    conn: &mut PgConnection,
    asm_ids: &[i64],
) -> Result<std::collections::HashMap<i64, i64>, AppError> {
    use std::collections::HashMap;
    let mut out: HashMap<i64, i64> = HashMap::new();
    if asm_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT assembly_id, COUNT(*) \
         FROM t_part WHERE assembly_id = ANY($1) AND deleted_at IS NULL \
         GROUP BY assembly_id",
    )
    .bind(asm_ids)
    .fetch_all(&mut *conn)
    .await?;
    for (aid, cnt) in rows {
        out.insert(aid, cnt);
    }
    Ok(out)
}

/// `TAssembly` → `PartListItem` 投影（ASSEMBLY-only 模式用）。
///
/// 与原 `part::service::crud::project_assembly_to_part_list_item` 逻辑一致：
/// `unit_price` / `total_price` 走 `Option<Decimal>` → `Decimal` 兜底为 `Decimal::ZERO`
/// （与 `t_part` NOT NULL DEFAULT 0 + frontend Zod `z.string()` 三方契约对齐）。
fn project_assembly_to_part_list_item(a: TAssembly) -> PartListItem {
    let unit_price: Option<Decimal> = a.unit_price;
    let total_price: Option<Decimal> = a.total_price;
    PartListItem {
        id: a.id,
        serial_no: a.serial_no,
        name: a.name,
        drawing_no: a.drawing_no,
        applicant_name: a.applicant_name.unwrap_or_default(),
        quantity: a.quantity,
        request_date: a.request_date,
        planned_delivery_date: a.planned_delivery_date,
        customer_id: a.customer_id,
        assembly_id: None,
        status: a.status,
        is_urgent: a.is_urgent,
        order_no: a.order_no,
        system_delivery_date: a.system_delivery_date,
        note: a.note,
        unit_price: unit_price.unwrap_or(Decimal::ZERO),
        total_price: total_price.unwrap_or(Decimal::ZERO),
        version: a.version,
        created_at: a.created_at,
        created_by: a.created_by,
        updated_at: a.updated_at,
        updated_by: a.updated_by,
        deleted_at: a.deleted_at,
        process_chain_id: None, // t_assembly 不持 process_chain_id
        customer_name: None,
        l1_customer_name: None,
        location: None,
        holder_name: None,
        row_type: Some("ASSEMBLY".to_string()),
        has_children: false, // 由 caller 用 child_count 覆盖
        child_count: None,   // 由 caller 用 fetch_child_counts 覆盖
        // 2026-09-29 合并：master `1e267ac` 加的 PartListItem 新字段 `has_cnc_program`
        // （part/vo/part.rs:200-207），t_assembly 不持 G_CODE 程序概念，ALL 模式下默认 false。
        // 注：com/union_list 是 feat 新增域，feat 未跟到此字段；fix-up 在合并 master 时加。
        has_cnc_program: false,
    }
}

/// `UnionListRow` → `PartListItem` 投影（ALL 模式用）。
///
/// 与 `project_assembly_to_part_list_item` 形态一致，但接收 SQL 层 UNION 投影后
/// 的扁平 `UnionListRow`；`row_type` 字段直接复用 SQL 写死的字面量。
fn union_row_to_part_list_item(r: UnionListRow) -> PartListItem {
    let row_type = Some(r.row_type.clone());
    PartListItem {
        id: r.id,
        serial_no: r.serial_no,
        name: r.name,
        drawing_no: r.drawing_no,
        applicant_name: r.applicant_name.unwrap_or_default(),
        quantity: r.quantity,
        request_date: r.request_date,
        planned_delivery_date: r.planned_delivery_date,
        customer_id: r.customer_id,
        assembly_id: r.assembly_id,
        status: r.status,
        is_urgent: r.is_urgent,
        order_no: r.order_no,
        system_delivery_date: r.system_delivery_date,
        note: r.note,
        unit_price: r.unit_price.unwrap_or(Decimal::ZERO),
        total_price: r.total_price.unwrap_or(Decimal::ZERO),
        version: r.version,
        created_at: r.created_at,
        created_by: r.created_by,
        updated_at: r.updated_at,
        updated_by: r.updated_by,
        deleted_at: r.deleted_at,
        process_chain_id: r.process_chain_id,
        customer_name: None,
        l1_customer_name: None,
        location: None,
        holder_name: None,
        row_type,
        has_children: false,
        child_count: None,
        // 2026-09-29 合并：同上，`has_cnc_program` 默认 false。
        has_cnc_program: false,
    }
}
