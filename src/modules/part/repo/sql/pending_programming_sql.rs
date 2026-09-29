//! part 域 SQL 真源 —— `pending-programming` 列表专用方法（2026-09-29 改造）
//!
//! ## 设计动机
//! 原 `list_pending_programming` 复用 [`super::super::list_with_filters`] +
//! `status='PROGRAMMING'` 谓词；2026-09-29 任务书要求基于 `t_process.is_cnc` 列的
//! 链上/货架过滤 + 新 query 参数 `has_cnc_program?`（Tab 切换）+ 出参含
//! `has_cnc_program: bool` 派生字段（`EXISTS t_part_file.kind='G_CODE'`）。
//!
//! 单独模块的原因：`list_with_filters` 是通用 list 路径（17 个 caller 共享），
//! 不在 `t_part` SELECT 列表加 `has_cnc_program` EXISTS 会破坏 ORDER BY 索引利用；
//! 而本任务要做的「链上 CNC step / 货架 CNC / 已编程 Tab」组合谓词与通用 list 无关。
//!
//! ## 谓词语义
//! 1. `t_part.deleted_at IS NULL`
//! 2. `t_part.status IN ('PENDING','IN_PROCESS','PROGRAMMING')`
//!    （历史 PROGRAMMING 状态数据仍允许消化）
//! 3. 满足以下之一：
//!    a. 工艺链上含 `t_process.is_cnc = TRUE` 的 step
//!    b. 当前 active 批次所在货架 `t_shelf_process` 关联到 `t_process.is_cnc = TRUE`
//!       且批次状态在 `{PENDING, IN_PROCESS, PROGRAMMING}`
//! 4. 可选 `has_cnc_program` 过滤（`EXISTS t_part_file.kind='G_CODE'`）
//! 5. 可选 `keyword` 模糊匹配（`name / drawing_no / serial_no` ILIKE '%kw%'）
//!
//! ## 排序 / 分页
//! - 默认排序 `p.planned_delivery_date ASC NULLS LAST, p.id DESC`
//! - `limit` ∈ [1, 500]，clamp；`offset` ≥ 0
//!
//! ## 行结构
//! `PendingProgrammingItem`（在 `super::super::super::vo::part` 域外单独定义
//! 见 `part/vo/part.rs::PendingProgrammingItem`）含完整 `TPart` 列 + `has_cnc_program` 派生。
//!
//! 其它 list 端点继续走通用 `list_with_filters`；`has_cnc_program` 字段默认 `false`
//! （service 层不在那里 enrich，避免 N+1）。
//!
//! ## SQL 拼接策略
//! 走 `sqlx::QueryBuilder`（与 `part_sql.rs::list_with_filters` 同形）——
//! 动态 SQL 用 `push_bind` 拼装参数，所有白名单 / 固定字面量（列名、谓词骨架）
//! 通过 `push(" ...")` 嵌入。`format!` + `query_as` 触发的 `SqlSafeStr` 限制
//! 由 QueryBuilder 天然规避。

use sqlx::{PgConnection, QueryBuilder};

use crate::modules::part::model::TPart;

use super::PartRepo;

/// `pending-programming` 列表入参（独立 struct，避免污染通用 [`PartListFilters`]）。
///
/// 与 [`crate::modules::part::dto_crud::PendingProgrammingQuery`] 同语义，由
/// `PartService::list_pending_programming` 显式映射（已规范化 keyword / sort_by
/// / sort_dir / limit / offset / has_cnc_program）。
#[derive(Debug, Clone, Default)]
pub struct PendingProgrammingFilters {
    pub keyword: Option<String>,
    pub sort_by: String,
    pub sort_dir: String,
    pub limit: i64,
    pub offset: i64,
    pub has_cnc_program: Option<bool>,
}

/// `pending-programming` 列表行（`TPart` 全字段 + `has_cnc_program` 派生）。
#[derive(Debug, Clone)]
pub struct PendingProgrammingItem {
    pub part: TPart,
    pub has_cnc_program: bool,
}

impl PartRepo {
    /// `pending-programming` 列表查询。
    ///
    /// 谓词见模块 docstring；service 层用 `items` 渲染 + `count_*` 填分页 meta。
    pub async fn list_pending_programming_with_cnc_filter(
        conn: &mut PgConnection,
        f: &PendingProgrammingFilters,
    ) -> Result<Vec<PendingProgrammingItem>, sqlx::Error> {
        let order_col = match f.sort_by.as_str() {
            "CREATED_AT" => "p.created_at",
            "UPDATED_AT" => "p.updated_at",
            "PLANNED_DELIVERY_DATE" => "p.planned_delivery_date",
            "REQUEST_DATE" => "p.request_date",
            "SERIAL_NO" => "p.serial_no",
            "DRAWING_NO" => "p.drawing_no",
            "NAME" => "p.name",
            _ => "p.planned_delivery_date",
        };
        let order_dir = if f.sort_dir.eq_ignore_ascii_case("DESC") {
            "DESC"
        } else {
            "ASC"
        };

        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT \
             p.id, p.serial_no, p.name, p.drawing_no, p.applicant_name, p.quantity, \
             p.request_date, p.planned_delivery_date, \
             p.customer_id, p.assembly_id, p.status, p.is_urgent, \
             p.next_process_id, \
             p.order_no, p.system_delivery_date, p.note, \
             p.unit_price, p.total_price, \
             p.version, p.created_at, p.created_by, p.updated_at, p.updated_by, \
             p.deleted_at, p.process_chain_id, \
             EXISTS (SELECT 1 FROM t_part_file pf \
                     WHERE pf.part_id = p.id AND pf.kind = 'G_CODE' \
                       AND pf.deleted_at IS NULL) AS has_cnc_program \
             FROM t_part p \
             WHERE p.deleted_at IS NULL \
               AND p.status IN ('PENDING','IN_PROCESS','PROGRAMMING') \
               AND ( \
                 EXISTS (SELECT 1 FROM t_process_chain_step s \
                         JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL \
                         WHERE s.chain_id = p.process_chain_id \
                           AND s.deleted_at IS NULL \
                           AND pr.is_cnc = TRUE) \
                 OR EXISTS (SELECT 1 FROM t_part_batch pb \
                            JOIN t_shelf_process sp \
                              ON sp.shelf_id = pb.current_holder_id AND sp.deleted_at IS NULL \
                            JOIN t_process pr \
                              ON pr.id = sp.process_id AND pr.deleted_at IS NULL \
                            WHERE pb.part_id = p.id \
                              AND pb.deleted_at IS NULL \
                              AND pb.status IN ('PENDING','IN_PROCESS','PROGRAMMING') \
                              AND pr.is_cnc = TRUE) \
               )",
        );

        // has_cnc_program 过滤：Option<bool> 三态
        qb.push(" AND (");
        qb.push_bind(f.has_cnc_program);
        qb.push(
            "::bool IS NULL OR EXISTS (SELECT 1 FROM t_part_file pf \
                WHERE pf.part_id = p.id AND pf.kind = 'G_CODE' \
                  AND pf.deleted_at IS NULL) = ",
        );
        qb.push_bind(f.has_cnc_program);
        qb.push(")");

        // keyword 模糊匹配
        if let Some(kw) = f
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let pat = format!("%{}%", kw);
            qb.push(" AND (p.name ILIKE ")
                .push_bind(pat.clone())
                .push(" OR p.drawing_no ILIKE ")
                .push_bind(pat.clone())
                .push(" OR p.serial_no ILIKE ")
                .push_bind(pat)
                .push(")");
        }

        qb.push(format!(
            " ORDER BY {order_col} {order_dir} NULLS LAST, p.id DESC LIMIT "
        ));
        qb.push_bind(f.limit);
        qb.push(" OFFSET ");
        qb.push_bind(f.offset);

        let rows: Vec<PendingProgrammingItemRow> = qb
            .build_query_as::<PendingProgrammingItemRow>()
            .fetch_all(conn)
            .await?;

        Ok(rows
            .into_iter()
            .map(|r| PendingProgrammingItem {
                part: TPart {
                    id: r.id,
                    serial_no: r.serial_no,
                    name: r.name,
                    drawing_no: r.drawing_no,
                    applicant_name: r.applicant_name,
                    quantity: r.quantity,
                    request_date: r.request_date,
                    planned_delivery_date: r.planned_delivery_date,
                    customer_id: r.customer_id,
                    assembly_id: r.assembly_id,
                    status: r.status,
                    is_urgent: r.is_urgent,
                    next_process_id: r.next_process_id,
                    order_no: r.order_no,
                    system_delivery_date: r.system_delivery_date,
                    note: r.note,
                    unit_price: r.unit_price,
                    total_price: r.total_price,
                    version: r.version,
                    created_at: r.created_at,
                    created_by: r.created_by,
                    updated_at: r.updated_at,
                    updated_by: r.updated_by,
                    deleted_at: r.deleted_at,
                    process_chain_id: r.process_chain_id,
                },
                has_cnc_program: r.has_cnc_program,
            })
            .collect())
    }

    /// `pending-programming` 列表计数（同 `list_pending_programming_with_cnc_filter` 的 WHERE）。
    pub async fn count_pending_programming_with_cnc_filter(
        conn: &mut PgConnection,
        f: &PendingProgrammingFilters,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT COUNT(*)::bigint AS n FROM t_part p \
             WHERE p.deleted_at IS NULL \
               AND p.status IN ('PENDING','IN_PROCESS','PROGRAMMING') \
               AND ( \
                 EXISTS (SELECT 1 FROM t_process_chain_step s \
                         JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL \
                         WHERE s.chain_id = p.process_chain_id \
                           AND s.deleted_at IS NULL \
                           AND pr.is_cnc = TRUE) \
                 OR EXISTS (SELECT 1 FROM t_part_batch pb \
                            JOIN t_shelf_process sp \
                              ON sp.shelf_id = pb.current_holder_id AND sp.deleted_at IS NULL \
                            JOIN t_process pr \
                              ON pr.id = sp.process_id AND pr.deleted_at IS NULL \
                            WHERE pb.part_id = p.id \
                              AND pb.deleted_at IS NULL \
                              AND pb.status IN ('PENDING','IN_PROCESS','PROGRAMMING') \
                              AND pr.is_cnc = TRUE) \
               )",
        );

        qb.push(" AND (");
        qb.push_bind(f.has_cnc_program);
        qb.push(
            "::bool IS NULL OR EXISTS (SELECT 1 FROM t_part_file pf \
                WHERE pf.part_id = p.id AND pf.kind = 'G_CODE' \
                  AND pf.deleted_at IS NULL) = ",
        );
        qb.push_bind(f.has_cnc_program);
        qb.push(")");

        if let Some(kw) = f
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let pat = format!("%{}%", kw);
            qb.push(" AND (p.name ILIKE ")
                .push_bind(pat.clone())
                .push(" OR p.drawing_no ILIKE ")
                .push_bind(pat.clone())
                .push(" OR p.serial_no ILIKE ")
                .push_bind(pat)
                .push(")");
        }

        let (n,): (i64,) = qb.build_query_as().fetch_one(conn).await?;
        Ok(n)
    }
}

/// `list_pending_programming_with_cnc_filter` 行结构（FromRow）：
/// 27 列 TPart + 1 列 has_cnc_program。手动 `#[derive(FromRow)]` 而非 `query_as!`，
/// 避免宏在 query! cache 中固化（SQL 是动态拼装的）。
#[derive(sqlx::FromRow)]
struct PendingProgrammingItemRow {
    // t_part 27 列（与 part_sql.rs TPart 对齐）
    id: i64,
    serial_no: Option<String>,
    name: String,
    drawing_no: String,
    applicant_name: String,
    quantity: i32,
    request_date: chrono::NaiveDate,
    planned_delivery_date: chrono::NaiveDate,
    customer_id: i64,
    assembly_id: Option<i64>,
    status: String,
    is_urgent: bool,
    next_process_id: Option<i64>,
    order_no: Option<String>,
    system_delivery_date: Option<chrono::NaiveDate>,
    note: Option<String>,
    unit_price: rust_decimal::Decimal,
    total_price: rust_decimal::Decimal,
    version: i32,
    created_at: chrono::NaiveDateTime,
    created_by: Option<i64>,
    updated_at: chrono::NaiveDateTime,
    updated_by: Option<i64>,
    deleted_at: Option<chrono::NaiveDateTime>,
    process_chain_id: Option<i64>,
    has_cnc_program: bool,
}
