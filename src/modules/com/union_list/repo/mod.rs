//! com::union_list 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-29 新增）
//! - `sql.rs`：union ALL + pushdown 的单段 SQL；固有静态方法 `list_union_all_with_filters`。
//! - `mod.rs`（本文件）：对外暴露瘦 trait `UnionListRepoTrait`（仅 ALL 模式单方法），
//!   并直接 `impl for &mut PgConnection` —— handler/service 借 `&mut *tx` / `&mut *conn` 即可。
//!
//! ## 为什么单方法 trait 而非胖 trait
//! com::union_list 当前只有 1 个对外方法（`list_union_all_with_filters`），不像
//! part 域需要 37 个方法合成 trait。PART / ASSEMBLY 单段模式直接走
//! `PartRepoTrait::list_with_filters` / `AssemblyRepoTrait::list_with_filters`，
//! 不经过本 trait。
//!
//! ## automock
//! trait 上加 `#[cfg_attr(test, mockall::automock)]` 预留，便于 service 单测注入。
//! 当前 service 无内联 mod tests（与 `com/customer` 范本一致）。

use async_trait::async_trait;
use sqlx::PgConnection;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与行类型，让上层继续用
// `super::repo::{UnionListRow, UnionListRepo}` 这种路径不破。
pub use sql::UnionListRepo;
// `UnionListRow` 是 SQL 行结构，仅 com::union_list 域内部使用；
// service 层直接 `use super::super::repo::UnionListRow` 即可（不需要 pub 暴露）。

/// 联合查询行（part 段 + asm 段共用形态，列对齐 `PartListItem` 写大集合）。
///
/// 字段顺序与 `com/union_list/repo/sql.rs::list_union_all_with_filters` 中
/// SQL 投影顺序一一对应；改动任何一边都要同步另一边。
#[derive(Debug, Clone)]
pub struct UnionListRow {
    pub id: i64,
    pub drawing_no: String,
    pub name: String,
    pub applicant_name: Option<String>,
    pub customer_id: i64,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    pub is_urgent: bool,
    pub status: String,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
    pub serial_no: Option<String>,
    pub quantity: i32,
    pub unit_price: Option<rust_decimal::Decimal>,
    pub total_price: Option<rust_decimal::Decimal>,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub note: Option<String>,
    pub assembly_id: Option<i64>,
    pub next_process_id: Option<i64>,
    pub process_chain_id: Option<i64>,
    /// 行类型字面量：`"PART"` / `"ASSEMBLY"`，由 SQL 层 UNION 投影强制写死。
    pub row_type: String,
}

/// union_list 域数据访问 trait（1 方法）。
///
/// `<'a>` 显式生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait UnionListRepoTrait: Send {
    /// 跨 `t_part` + `t_assembly` 段 UNION ALL 合并查询，每段 LIMIT (offset+limit)
    /// pushdown，外层 ORDER BY + LIMIT + OFFSET。
    ///
    /// 参数：
    /// - `filters`：跨段共享筛选（customer_ids / status / statuses / is_urgent /
    ///   keyword / locations / holder_ids）；part 段 `assembly_id IS NULL` 守卫
    ///   在 SQL 内固定追加。
    /// - `planned_delivery_date_from` / `planned_delivery_date_to`（2026-09-30
    ///   新增）：日期窗口（`YYYY-MM-DD`），段内 `WHERE planned_delivery_date >=
    ///   $X AND <= $Y`；`None` 端不参与。
    /// - 4 文本 ILIKE pattern（2026-09-30 新增）：`drawing_no_pat` / `name_pat`
    ///   / `order_no_pat` / `serial_no_pat`（已 `%x%` 预格式化），段内
    ///   `WHERE <col> ILIKE $X`；`None` 端不参与。
    /// - 4 日期窗口（2026-09-30 新增）：`request_date_from/to` +
    ///   `system_delivery_date_from/to`，段内 `WHERE <col> >= $X AND <= $Y`；
    ///   `None` 端不参与。
    /// - 2 IS NULL 三态（2026-09-30 新增）：`order_no_is_null` /
    ///   `system_delivery_date_is_null`，段内 `WHERE <col> IS [NOT] NULL`
    ///   或 `order_no` 含空串语义对齐 PR-F 2026-08-11（空串视为『未填』/
    ///   NULL 同义）；`None` 端不参与。
    /// - `pushdown_limit`：每段 SQL 内 `LIMIT (pushdown_limit) OFFSET 0`；用户
    ///   输入 = `offset + limit`。
    /// - `limit` / `offset`：外层分页切片。
    ///
    /// 返回：`Vec<UnionListRow>`，已按 `filters.sort_by + sort_dir` 全局排序 +
    /// 取前 N 行（pushdown 保证结果正确性，详见 plan §3）。
    #[allow(clippy::too_many_arguments)]
    async fn list_union_all_with_filters<'a, 'b>(
        &mut self,
        customer_ids: &'a [i64],
        status: Option<&'a str>,
        statuses: &'a [String],
        is_urgent: Option<bool>,
        keyword: Option<&'a str>,
        locations: &'a [String],
        holder_ids: &'a [i64],
        sort_by: &'a str,
        sort_dir: &'a str,
        pushdown_limit: i64,
        limit: i64,
        offset: i64,
        // 2026-09-30 新增：日期窗口过滤（part/asm 段内 WHERE）。
        planned_delivery_date_from: Option<chrono::NaiveDate>,
        planned_delivery_date_to: Option<chrono::NaiveDate>,
        // 2026-09-30 新增：4 文本 ILIKE pattern（part/asm 段内 ILIKE）。
        drawing_no_pat: Option<&'a str>,
        name_pat: Option<&'a str>,
        order_no_pat: Option<&'a str>,
        serial_no_pat: Option<&'a str>,
        // 2026-09-30 新增：4 日期窗口（part/asm 段内 `>=`/`<=`）。
        request_date_from: Option<chrono::NaiveDate>,
        request_date_to: Option<chrono::NaiveDate>,
        system_delivery_date_from: Option<chrono::NaiveDate>,
        system_delivery_date_to: Option<chrono::NaiveDate>,
        // 2026-09-30 新增：2 IS NULL 三态（part/asm 段内 `IS [NOT] NULL`；
        // `order_no` 含空串语义对齐 PR-F 2026-08-11）。
        order_no_is_null: Option<bool>,
        system_delivery_date_is_null: Option<bool>,
    ) -> Result<Vec<UnionListRow>, sqlx::Error>;
}

/// `UnionListRepo` 直接对 `&mut PgConnection` 实现：handler/service 借
/// `&mut *tx` / `&mut *conn` 即可喂给 sql 方法，零转发壳。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`&mut **self` 即 reborrow 出 `&mut PgConnection`。
#[async_trait]
impl UnionListRepoTrait for &mut PgConnection {
    async fn list_union_all_with_filters<'b, 'c>(
        &mut self,
        customer_ids: &'b [i64],
        status: Option<&'b str>,
        statuses: &'b [String],
        is_urgent: Option<bool>,
        keyword: Option<&'b str>,
        locations: &'b [String],
        holder_ids: &'b [i64],
        sort_by: &'b str,
        sort_dir: &'b str,
        pushdown_limit: i64,
        limit: i64,
        offset: i64,
        // 2026-09-30 新增：日期窗口过滤透传。
        planned_delivery_date_from: Option<chrono::NaiveDate>,
        planned_delivery_date_to: Option<chrono::NaiveDate>,
        // 2026-09-30 新增：4 文本 ILIKE pattern 透传。
        drawing_no_pat: Option<&'b str>,
        name_pat: Option<&'b str>,
        order_no_pat: Option<&'b str>,
        serial_no_pat: Option<&'b str>,
        // 2026-09-30 新增：4 日期窗口透传。
        request_date_from: Option<chrono::NaiveDate>,
        request_date_to: Option<chrono::NaiveDate>,
        system_delivery_date_from: Option<chrono::NaiveDate>,
        system_delivery_date_to: Option<chrono::NaiveDate>,
        // 2026-09-30 新增：2 IS NULL 三态透传。
        order_no_is_null: Option<bool>,
        system_delivery_date_is_null: Option<bool>,
    ) -> Result<Vec<UnionListRow>, sqlx::Error> {
        UnionListRepo::list_union_all_with_filters(
            &mut **self,
            customer_ids,
            status,
            statuses,
            is_urgent,
            keyword,
            locations,
            holder_ids,
            sort_by,
            sort_dir,
            pushdown_limit,
            limit,
            offset,
            planned_delivery_date_from,
            planned_delivery_date_to,
            drawing_no_pat,
            name_pat,
            order_no_pat,
            serial_no_pat,
            request_date_from,
            request_date_to,
            system_delivery_date_from,
            system_delivery_date_to,
            order_no_is_null,
            system_delivery_date_is_null,
        )
        .await
    }
}
