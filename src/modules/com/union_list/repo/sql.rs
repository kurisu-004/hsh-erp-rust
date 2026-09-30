//! com::union_list 域数据访问（SQL 真源，2026-09-29 新增）
//!
//! 对应 plan §3：跨 `t_part` + `t_assembly` 段 UNION ALL，每段 LIMIT (offset+limit)
//! pushdown，外层 ORDER BY + LIMIT + OFFSET —— 修分页 bug（原 ALL 模式
//! `segment_limit.clamp(1,200)` 在 deep offset 时返回空集）。
//!
//! ## SQL 形态（单段固定 SQL + 链式 .bind()）
//!
//! 不再走 `QueryBuilder::into_sql()` 拼接两段（review round-1 C-1/C-2/C-3
//! 三个 CRITICAL bug：`into_sql()` 丢 `self.arguments`、两 builder 撞 `$1`、
//! asm 段残留重复 status 块）。改为单一 `format!` 拼 SQL 字符串 +
//! `sqlx::query(&sql).bind(...).bind(...)` 链；每个 placeholder 只 bind 一
//! 次，PostgreSQL 允许同一 `$N` 在 SQL 文本内多处引用。
//!
//! ```sql
//! WITH
//!   part_seg AS (
//!     SELECT <26 列> FROM t_part
//!     WHERE deleted_at IS NULL AND assembly_id IS NULL
//!       /* (cardinality($2) = 0 OR customer_id = ANY($2)) 等价短路 */
//!     ORDER BY <sort_col> NULLS LAST, id DESC
//!     LIMIT $1 OFFSET 0
//!   ),
//!   asm_seg AS (
//!     SELECT <26 列 + 3 NULLs + 'ASSEMBLY'::text> FROM t_assembly
//!     WHERE deleted_at IS NULL
//!     ORDER BY <sort_col> NULLS LAST, id DESC
//!     LIMIT $1 OFFSET 0
//!   )
//! SELECT <26 列>
//! FROM (SELECT * FROM part_seg UNION ALL SELECT * FROM asm_seg) AS u
//! ORDER BY <sort_col> NULLS LAST, id DESC
//! LIMIT $9 OFFSET $10;
//! ```
//!
//! ## placeholder → bind 映射（顺序固定）
//!
//! | `$N` | 类型                  | 来源参数       | 备注                              |
//! |------|-----------------------|----------------|-----------------------------------|
//! | 1    | `i64`                 | pushdown_limit | part / asm 段 LIMIT 共享          |
//! | 2    | `&[i64]`              | customer_ids   | 空切片 → `'{}'` cardinality 0 短路 |
//! | 3    | `Option<&str>`        | status         | None → NULL 短路                  |
//! | 4    | `&[String]`           | statuses       | 空切片 → cardinality 0 短路       |
//! | 5    | `Option<bool>`        | is_urgent      | None → NULL 短路                  |
//! | 6    | `Option<String>`      | keyword pattern | None → NULL 短路（已预格式化 %x%）|
//! | 7    | `&[String]`           | locations      | 仅 part 段 EXISTS                |
//! | 8    | `&[i64]`              | holder_ids     | 仅 part 段 EXISTS                |
//! | 9    | `i64`                 | limit          | 外层                              |
//! | 10   | `i64`                 | offset         | 外层                              |
//! | 11   | `Option<NaiveDate>`   | planned_delivery_date_from   | 2026-09-30 新增（part/asm 段 `>=`，外层 SQL 不引用）|
//! | 12   | `Option<NaiveDate>`   | planned_delivery_date_to     | 2026-09-30 新增（part/asm 段 `<=`，外层 SQL 不引用）|
//!
//! 2026-09-30 修订：日期 from/to 用 `$11`/`$12` 占位共享给 part/asm 段（PG 允
//! 许同 `$N` 在 SQL 文本内多处引用；外层 SQL 不消费这两个 placeholder，故不
//! 影响 `$9`/`$10` 语义）。
//!
//! ## 正确性论证
//! 每段内部按各自 sort_key 取前 `pushdown_limit = offset + limit` 行；外层
//! UNION ALL 后再做全局排序+分页，位置 `[offset, offset+limit)` 内的任何行
//! 必然来自某段的前 `(offset+limit)` 行（否则它不会进入 top-N），所以结果正确。
//!
//! ## 索引命中
//! - `customer_id` / `status` 同时命中 `ix_t_part_customer_status_delivery` /
//!   `ix_t_assembly_customer_status`（DDL 见 migrations/20260925000000_001_baseline.sql）。
//! - `keyword` 走 ILIKE 三列 OR（name / drawing_no / serial_no），DDL 上无
//!   trigram 索引，单段可能 seq scan；客户筛选缩窄后命中索引覆盖。
//!
//! ## 排序键白名单
//! CREATED_AT / UPDATED_AT / PLANNED_DELIVERY_DATE / REQUEST_DATE / DRAWING_NO /
//! NAME；`SERIAL_NO` 仅 t_part 独有 → 降级 CREATED_AT（与 PART-only 模式一致；
//! 服务层在 `parse_filters` 已收口）。
//!
//! ## 函数签名
//! - `list_union_all_with_filters`：接收 12 个扁平形参（避免
//!   `UnionListFilters<'a>` 跨 trait 边界传递），SQL 走 runtime
//!   `sqlx::query(&str).bind(...)` 链（动态 SQL 不能用 `query!` 编译期宏）。

use sqlx::{AssertSqlSafe, PgExecutor, Row};

use super::UnionListRow;

/// 排序键白名单 → SQL 列名。
///
/// 与 `part/repo/sql/part_sql.rs::list_with_filters` 共用同一白名单子集（去掉
/// `SERIAL_NO`，因 t_assembly 上无对应列）；非法值降级 `id`，与 part 域同形。
fn union_sort_col(sort_by: &str) -> &'static str {
    match sort_by {
        "CREATED_AT" => "created_at",
        "UPDATED_AT" => "updated_at",
        "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
        "REQUEST_DATE" => "request_date",
        "DRAWING_NO" => "drawing_no",
        "NAME" => "name",
        "SYSTEM_DELIVERY_DATE" => "system_delivery_date", // 2026-09-30 新增
        _ => "id",
    }
}

/// SQL 真源 ZST。trait 名为 `UnionListRepoTrait`（公共接口），ZST 仍名
/// `UnionListRepo`（与 customer 域 `CustomerRepo` 范本一致：跨模块静态调用
/// 方依赖 ZST 名）。
pub struct UnionListRepo;

impl UnionListRepo {
    /// 跨段 UNION ALL + pushdown + 外层分页。
    ///
    /// 排序方向：仅接受 `ASC`，其它视为 `DESC`（与 part 域 list 端点同形）。
    ///
    /// `pushdown_limit` = `offset + limit`（service 层算好传入），保证每段取够
    /// 全局排序所需的行。
    ///
    /// ## SQL 写法（2026-09-29 round-1 修复后；2026-09-30 增 date 过滤）
    ///
    /// 单一固定 SQL（`format!` 仅替换 `order_col` / `order_dir`，已白名单），
    /// 配 12 次 `.bind()` 链；每 placeholder 在 SQL 文本内可多次引用，PG 接
    /// 受同一 `$N` 多处出现，bind 次数与 placeholder 总数对应（12 个）。
    ///
    /// 2026-09-30 增 `planned_delivery_date_from/to: Option<NaiveDate>` 段内
    /// 过滤；两段（part_seg / asm_seg）共享 `$11`/`$12`，外层 SQL 不消费。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_union_all_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        customer_ids: &[i64],
        status: Option<&str>,
        statuses: &[String],
        is_urgent: Option<bool>,
        keyword: Option<&str>,
        locations: &[String],
        holder_ids: &[i64],
        sort_by: &str,
        sort_dir: &str,
        pushdown_limit: i64,
        limit: i64,
        offset: i64,
        // 2026-09-30 新增：日期窗口过滤（仅 part/asm 段内消费）。
        planned_delivery_date_from: Option<chrono::NaiveDate>,
        planned_delivery_date_to: Option<chrono::NaiveDate>,
    ) -> Result<Vec<UnionListRow>, sqlx::Error> {
        let order_col = union_sort_col(sort_by);
        let order_dir = if sort_dir.eq_ignore_ascii_case("ASC") {
            "ASC"
        } else {
            "DESC"
        };

        // 预格式化 keyword 为 ILIKE pattern；None → bind 为 NULL（被 `$6::text IS NULL` 短路）
        let keyword_pat: Option<String> = keyword.map(|k| format!("%{}%", k.trim()));

        // 单一固定 SQL（`{order_col}` / `{order_dir}` 已白名单校验，无注入风险）：
        // - part_seg / asm_seg 各自 WHERE；外层 UNION ALL + 排序 + 分页
        // - 段内共用 `$2` (customer_ids) / `$3` (status) / `$4` (statuses) /
        //   `$5` (is_urgent) / `$6` (keyword)；PG 允许同一 `$N` 多处引用
        // - 段内 `LIMIT $1` 共享 pushdown_limit
        // - locations / holder_ids 仅 part_seg EXISTS 消费（asm 不持 batch）
        // - keyword 在两段都用 name / drawing_no / serial_no 三列 ILIKE OR
        //   （与 PartRepo / AssemblyRepo::list_with_filters 同形 —— review
        //   round-1 MODERATE-4 已统一三列匹配语义）
        // - 2026-09-30 新增：`$11`/`$12` 日期窗口（part/asm 两段共享，外层不消费）
        let sql = format!(
            "WITH \
             part_seg AS ( \
               SELECT id, drawing_no, name, applicant_name, customer_id, \
                      request_date, planned_delivery_date, is_urgent, status, \
                      version, created_at, created_by, updated_at, updated_by, \
                      deleted_at, serial_no, quantity, unit_price, total_price, \
                      order_no, system_delivery_date, note, \
                      assembly_id, next_process_id, process_chain_id, \
                      'PART'::text AS row_type \
               FROM t_part \
               WHERE deleted_at IS NULL \
                 AND assembly_id IS NULL \
                 AND (cardinality($2::bigint[]) = 0 OR customer_id = ANY($2)) \
                 AND ($3::text IS NULL OR status = $3) \
                 AND (cardinality($4::text[]) = 0 OR status = ANY($4)) \
                 AND ($5::bool IS NULL OR is_urgent = $5) \
                 AND ($6::text IS NULL OR (name ILIKE $6 OR drawing_no ILIKE $6 OR serial_no ILIKE $6)) \
                 AND (cardinality($7::text[]) = 0 OR EXISTS ( \
                   SELECT 1 FROM t_part_batch pb \
                   WHERE pb.part_id = t_part.id \
                     AND pb.location = ANY($7) \
                     AND pb.deleted_at IS NULL \
                 )) \
                 AND (cardinality($8::bigint[]) = 0 OR EXISTS ( \
                   SELECT 1 FROM t_part_batch pb \
                   WHERE pb.part_id = t_part.id \
                     AND pb.current_holder_id = ANY($8) \
                     AND pb.deleted_at IS NULL \
                 )) \
                 AND ($11::date IS NULL OR planned_delivery_date >= $11) \
                 AND ($12::date IS NULL OR planned_delivery_date <= $12) \
               ORDER BY {order_col} {order_dir} NULLS LAST, id DESC \
               LIMIT $1 OFFSET 0 \
             ), \
             asm_seg AS ( \
               SELECT id, drawing_no, name, applicant_name, customer_id, \
                      request_date, planned_delivery_date, is_urgent, status, \
                      version, created_at, created_by, updated_at, updated_by, \
                      deleted_at, serial_no, quantity, unit_price, total_price, \
                      order_no, system_delivery_date, note, \
                      NULL::bigint AS assembly_id, \
                      NULL::bigint AS next_process_id, \
                      NULL::bigint AS process_chain_id, \
                      'ASSEMBLY'::text AS row_type \
               FROM t_assembly \
               WHERE deleted_at IS NULL \
                 AND (cardinality($2::bigint[]) = 0 OR customer_id = ANY($2)) \
                 AND ($3::text IS NULL OR status = $3) \
                 AND (cardinality($4::text[]) = 0 OR status = ANY($4)) \
                 AND ($5::bool IS NULL OR is_urgent = $5) \
                 AND ($6::text IS NULL OR (drawing_no ILIKE $6 OR name ILIKE $6 OR serial_no ILIKE $6)) \
                 AND ($11::date IS NULL OR planned_delivery_date >= $11) \
                 AND ($12::date IS NULL OR planned_delivery_date <= $12) \
               ORDER BY {order_col} {order_dir} NULLS LAST, id DESC \
               LIMIT $1 OFFSET 0 \
             ) \
             SELECT id, drawing_no, name, applicant_name, customer_id, \
                    request_date, planned_delivery_date, is_urgent, status, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, serial_no, quantity, unit_price, total_price, \
                    order_no, system_delivery_date, note, \
                    assembly_id, next_process_id, process_chain_id, row_type \
             FROM ( \
               SELECT * FROM part_seg \
               UNION ALL \
               SELECT * FROM asm_seg \
             ) AS u \
             ORDER BY {order_col} {order_dir} NULLS LAST, id DESC \
             LIMIT $9 OFFSET $10"
        );

        // 走 runtime sqlx::query（动态 SQL 不能用 `query!` 编译期宏）。
        // bind 顺序与 SQL 中 `$N` 编号一一对应；同一 `$N` 在 SQL 内多处引用
        // 只算一次 bind（PG 支持）。`sqlx 0.9` 的 `query` 仅 impl `SqlSafeStr`
        // for `&'static str`，动态 String 必须 `AssertSqlSafe(...)` 包一层
        // —— 安全审计：此处 `format!` 仅替换 `order_col` / `order_dir`（两者
        // 已在 `union_sort_col` / 上面 if-else 内白名单校验），零注入风险。
        let rows = sqlx::query(AssertSqlSafe(sql))
            .bind(pushdown_limit) // $1
            .bind(customer_ids) // $2
            .bind(status) // $3
            .bind(statuses) // $4
            .bind(is_urgent) // $5
            .bind(keyword_pat) // $6
            .bind(locations) // $7
            .bind(holder_ids) // $8
            .bind(limit) // $9
            .bind(offset) // $10
            .bind(planned_delivery_date_from) // $11 (2026-09-30 新增)
            .bind(planned_delivery_date_to) // $12 (2026-09-30 新增)
            .fetch_all(executor)
            .await?;
        rows.into_iter().map(row_to_union_list_row).collect()
    }
}

/// 把 `sqlx::Row` 解码到 `UnionListRow`（runtime `query_as` 不支持 derive）。
///
/// 列顺序与上方 `list_union_all_with_filters` 的外层 SELECT 一一对应（共 26 列）。
/// 列名与 t_part / t_assembly 的 DDL 一致；union 投影后 `row_type` 是字面量
/// `'PART'::text` / `'ASSEMBLY'::text`，由 SQL 层强制写死。
///
/// NUMERIC 列处理：t_part.unit_price / total_price 是 NUMERIC NOT NULL DEFAULT
/// 0（`Decimal`），t_assembly 同列是 NUMERIC nullable（`Option<Decimal>`）；
/// UNION 后整体 nullable。sqlx 0.9 + `rust_decimal` feature 直接把 NUMERIC
/// 解码到 `Decimal`（无 BigDecimal 中转），与 part 域 `TPart` / `TAssembly`
/// 同源。
fn row_to_union_list_row(row: sqlx::postgres::PgRow) -> Result<UnionListRow, sqlx::Error> {
    use rust_decimal::Decimal;

    Ok(UnionListRow {
        id: row.try_get("id")?,
        drawing_no: row.try_get("drawing_no")?,
        name: row.try_get("name")?,
        applicant_name: row.try_get("applicant_name")?,
        customer_id: row.try_get("customer_id")?,
        request_date: row.try_get("request_date")?,
        planned_delivery_date: row.try_get("planned_delivery_date")?,
        is_urgent: row.try_get("is_urgent")?,
        status: row.try_get("status")?,
        version: row.try_get("version")?,
        created_at: row.try_get("created_at")?,
        created_by: row.try_get("created_by")?,
        updated_at: row.try_get("updated_at")?,
        updated_by: row.try_get("updated_by")?,
        deleted_at: row.try_get("deleted_at")?,
        serial_no: row.try_get("serial_no")?,
        quantity: row.try_get("quantity")?,
        // UNION 投影后 nullable —— 走 Option<Decimal> 解码
        unit_price: row.try_get::<Option<Decimal>, _>("unit_price")?,
        total_price: row.try_get::<Option<Decimal>, _>("total_price")?,
        order_no: row.try_get("order_no")?,
        system_delivery_date: row.try_get("system_delivery_date")?,
        note: row.try_get("note")?,
        assembly_id: row.try_get("assembly_id")?,
        next_process_id: row.try_get("next_process_id")?,
        process_chain_id: row.try_get("process_chain_id")?,
        row_type: row.try_get("row_type")?,
    })
}
