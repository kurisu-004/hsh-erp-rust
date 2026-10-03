//! outsource 域数据访问（SQL 真源，零 diff 搬迁自 `repo.rs`）
//!
//! 对应 Python myERP/repository/outsource*.py。
//!
//! 实施约定：
//! - 全部 `sqlx::query_as::<_, T>` + `FromRow`（runtime，不依赖 `.sqlx/` 缓存）
//! - 软删：默认 `deleted_at IS NULL`；写 UPDATE 带 `WHERE id=$1 AND version=$2` 乐观锁
//! - 0 行由 service 转 409 / BIZ_VERSION_CONFLICT
//! - `&mut PgConnection` 走引用 `&mut *conn`，与 part / customer 域同形
//!
//! 2026-09-22 refactor（outsource 对齐 iam 事务分层范式）：
//! 从 `repo.rs` 平移到 `repo/sql.rs`，**所有 SQL 字符串零 diff**（`.sqlx/query-*.json`
//! 哈希不变）；`OutsourceRepoTrait` 在 `repo/mod.rs`，直接 `impl for &mut PgConnection`。
//! 3 个 ZST struct `OutsourceCompanyRepo` / `OutsourceQuoteRepo` /
//! `OutsourceShipmentRepo` 保持原名（trait 命名为 `OutsourceRepoTrait`，避开与
//! 单 ZST 名歧义；outsource 自封闭，无 cross-module 静态调用方）。

use sqlx::{AssertSqlSafe, PgExecutor};

use super::super::model::{
    NewOutsourceCompany, NewOutsourceCompanyProcess, NewOutsourceQuote, NewOutsourceQuoteEvent,
    NewOutsourceShipment, TOutsourceCompany, TOutsourceCompanyProcess, TOutsourceQuote,
    TOutsourceQuoteEvent, TOutsourceShipment,
};
use super::{
    OutsourceHeldBatchRow, OutsourceInFlightRow, OutsourcePoolCompanyRow, OutsourceQuotableRow,
    OutsourceSendableRow, OutsourceSentPartRow,
};

// ===========================================================================
// Company
// ===========================================================================

pub struct OutsourceCompanyRepo;

impl OutsourceCompanyRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceCompany>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceCompany>(
            "SELECT id, name, contact_name, contact_phone, address, is_active, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_company WHERE id = $1 AND ($2::bool OR deleted_at IS NULL)",
        )
        .bind(id)
        .bind(include_deleted)
        .fetch_optional(executor)
        .await
    }

    pub async fn get_by_name<'e, E: PgExecutor<'e>>(
        executor: E,
        name: &str,
    ) -> Result<Option<TOutsourceCompany>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceCompany>(
            "SELECT id, name, contact_name, contact_phone, address, is_active, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_company WHERE name = $1 AND deleted_at IS NULL",
        )
        .bind(name)
        .fetch_optional(executor)
        .await
    }

    pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        ids: &[i64],
    ) -> Result<Vec<TOutsourceCompany>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as::<_, TOutsourceCompany>(
            "SELECT id, name, contact_name, contact_phone, address, is_active, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_company WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(ids)
        .fetch_all(executor)
        .await
    }

    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        name_like: Option<&str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TOutsourceCompany>, sqlx::Error> {
        // 动态 SQL：name_like + is_active 可选；带 limit/offset
        let needle = name_like.map(|s| s.trim()).filter(|s| !s.is_empty());
        let pat = needle.map(|n| format!("%{}%", n));
        sqlx::query_as::<_, TOutsourceCompany>(
            "SELECT id, name, contact_name, contact_phone, address, is_active, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_company \
             WHERE deleted_at IS NULL \
               AND ($1::text IS NULL OR name ILIKE $1) \
               AND ($2::bool IS NULL OR is_active = $2) \
             ORDER BY id ASC LIMIT $3 OFFSET $4",
        )
        .bind(pat)
        .bind(is_active)
        .bind(limit)
        .bind(offset)
        .fetch_all(executor)
        .await
    }

    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        name_like: Option<&str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        let needle = name_like.map(|s| s.trim()).filter(|s| !s.is_empty());
        let pat = needle.map(|n| format!("%{}%", n));
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_outsource_company \
             WHERE deleted_at IS NULL \
               AND ($1::text IS NULL OR name ILIKE $1) \
               AND ($2::bool IS NULL OR is_active = $2)",
        )
        .bind(pat)
        .bind(is_active)
        .fetch_one(executor)
        .await?;
        Ok(n)
    }

    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        new: NewOutsourceCompany,
    ) -> Result<TOutsourceCompany, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceCompany>(
            "INSERT INTO t_outsource_company \
             (id, name, contact_name, contact_phone, address, is_active, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $7) \
             RETURNING id, name, contact_name, contact_phone, address, is_active, version, \
                       created_at, created_by, updated_at, updated_by, deleted_at",
        )
        .bind(new.id)
        .bind(&new.name)
        .bind(&new.contact_name)
        .bind(&new.contact_phone)
        .bind(&new.address)
        .bind(new.is_active)
        .bind(new.created_by)
        .fetch_one(executor)
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        name: Option<&str>,
        contact_name: Option<Option<&str>>,
        contact_phone: Option<Option<&str>>,
        address: Option<Option<&str>>,
        is_active: Option<bool>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_contact_name = contact_name.is_some();
        let new_contact_name = contact_name.flatten();
        let set_contact_phone = contact_phone.is_some();
        let new_contact_phone = contact_phone.flatten();
        let set_address = address.is_some();
        let new_address = address.flatten();
        let r = sqlx::query(
            "UPDATE t_outsource_company SET \
             name           = COALESCE($3::varchar, name), \
             contact_name   = CASE WHEN $4::bool THEN $5::varchar ELSE contact_name END, \
             contact_phone  = CASE WHEN $6::bool THEN $7::varchar ELSE contact_phone END, \
             address        = CASE WHEN $8::bool THEN $9::varchar ELSE address END, \
             is_active      = COALESCE($10::bool, is_active), \
             version        = version + 1, \
             updated_at     = now(), \
             updated_by     = $11 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(version)
        .bind(name)
        .bind(set_contact_name)
        .bind(new_contact_name)
        .bind(set_contact_phone)
        .bind(new_contact_phone)
        .bind(set_address)
        .bind(new_address)
        .bind(is_active)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_company SET deleted_at = now(), version = version + 1, \
             updated_at = now(), updated_by = $3 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(version)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }
}

// ===========================================================================
// CompanyProcess (junction)
// ===========================================================================

pub struct OutsourceCompanyProcessRepo;

impl OutsourceCompanyProcessRepo {
    pub async fn list_by_company<'e, E: PgExecutor<'e>>(
        executor: E,
        company_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TOutsourceCompanyProcess>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceCompanyProcess>(
            "SELECT id, outsource_company_id, process_id, sort_order, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_company_process \
             WHERE outsource_company_id = $1 AND ($2::bool OR deleted_at IS NULL) \
             ORDER BY sort_order ASC, id ASC",
        )
        .bind(company_id)
        .bind(include_deleted)
        .fetch_all(executor)
        .await
    }

    /// 反查能做该 process 的所有公司 id（含 soft-deleted；caller 自筛 is_active）。
    pub async fn list_company_ids_by_process<'e, E: PgExecutor<'e>>(
        executor: E,
        process_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT outsource_company_id FROM t_outsource_company_process \
             WHERE process_id = $1 AND deleted_at IS NULL",
        )
        .bind(process_id)
        .fetch_all(executor)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        new: NewOutsourceCompanyProcess,
    ) -> Result<TOutsourceCompanyProcess, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceCompanyProcess>(
            "INSERT INTO t_outsource_company_process \
             (id, outsource_company_id, process_id, sort_order, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $5) \
             RETURNING id, outsource_company_id, process_id, sort_order, version, \
                       created_at, created_by, updated_at, updated_by, deleted_at",
        )
        .bind(new.id)
        .bind(new.outsource_company_id)
        .bind(new.process_id)
        .bind(new.sort_order)
        .bind(new.created_by)
        .fetch_one(executor)
        .await
    }

    pub async fn soft_delete_by_company<'e, E: PgExecutor<'e>>(
        executor: E,
        company_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_company_process SET deleted_at = now(), version = version + 1, \
             updated_at = now(), updated_by = $2 \
             WHERE outsource_company_id = $1 AND deleted_at IS NULL",
        )
        .bind(company_id)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }
}

// ===========================================================================
// Quote
// ===========================================================================

pub struct OutsourceQuoteRepo;

impl OutsourceQuoteRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceQuote>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceQuote>(
            "SELECT id, part_id, outsource_company_id, process_id, price, note, status, \
             submitted_at, reviewed_at, review_note, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_quote WHERE id = $1 AND ($2::bool OR deleted_at IS NULL)",
        )
        .bind(id)
        .bind(include_deleted)
        .fetch_optional(executor)
        .await
    }

    pub async fn get_active_for_tuple<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        company_id: i64,
        process_id: i64,
    ) -> Result<Option<TOutsourceQuote>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceQuote>(
            "SELECT id, part_id, outsource_company_id, process_id, price, note, status, \
             submitted_at, reviewed_at, review_note, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_quote \
             WHERE part_id = $1 AND outsource_company_id = $2 AND process_id = $3 \
               AND deleted_at IS NULL \
               AND status NOT IN ('REJECTED') \
             LIMIT 1",
        )
        .bind(part_id)
        .bind(company_id)
        .bind(process_id)
        .fetch_optional(executor)
        .await
    }

    pub async fn list_all_approved<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<TOutsourceQuote>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceQuote>(
            "SELECT id, part_id, outsource_company_id, process_id, price, note, status, \
             submitted_at, reviewed_at, review_note, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_quote \
             WHERE deleted_at IS NULL AND status = 'APPROVED' \
             ORDER BY part_id ASC, created_at DESC, id DESC",
        )
        .fetch_all(executor)
        .await
    }

    pub async fn list_active_by_part_process<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        process_id: i64,
        exclude_id: i64,
    ) -> Result<Vec<TOutsourceQuote>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceQuote>(
            "SELECT id, part_id, outsource_company_id, process_id, price, note, status, \
             submitted_at, reviewed_at, review_note, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_quote \
             WHERE deleted_at IS NULL AND part_id = $1 AND process_id = $2 \
               AND id <> $3 AND status IN ('SUBMITTED', 'APPROVED')",
        )
        .bind(part_id)
        .bind(process_id)
        .bind(exclude_id)
        .fetch_all(executor)
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        status: Option<&str>,
        statuses: &[String],
        part_id: Option<i64>,
        part_ids_in: &[i64],
        outsource_company_id: Option<i64>,
        sort_by: &str,
        sort_dir: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TOutsourceQuote>, sqlx::Error> {
        // 单一固定 ORDER BY（避免动态 SQL 字符串）：用 case 表达式选列
        sqlx::query_as::<_, TOutsourceQuote>(
            "SELECT id, part_id, outsource_company_id, process_id, price, note, status, \
             submitted_at, reviewed_at, review_note, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_quote \
             WHERE deleted_at IS NULL \
               AND ($1::text IS NULL OR status = $1) \
               AND (cardinality($2::text[]) = 0 OR status = ANY($2)) \
               AND ($3::bigint IS NULL OR part_id = $3) \
               AND (cardinality($4::bigint[]) = 0 OR part_id = ANY($4)) \
               AND ($5::bigint IS NULL OR outsource_company_id = $5) \
             ORDER BY \
               CASE WHEN $6::text = 'PRICE' AND $7::text = 'DESC' THEN price END DESC NULLS LAST, \
               CASE WHEN $6::text = 'PRICE' AND $7::text <> 'DESC' THEN price END ASC NULLS LAST, \
               CASE WHEN $6::text = 'REVIEWED_AT' AND $7::text = 'ASC' THEN reviewed_at END ASC NULLS LAST, \
               CASE WHEN $6::text = 'REVIEWED_AT' AND $7::text <> 'ASC' THEN reviewed_at END DESC NULLS LAST, \
               CASE WHEN $6::text = 'CREATED_AT' AND $7::text = 'ASC' THEN created_at END ASC, \
               created_at DESC, \
               id DESC \
             LIMIT $8 OFFSET $9",
        )
        .bind(status)
        .bind(statuses)
        .bind(part_id)
        .bind(part_ids_in)
        .bind(outsource_company_id)
        .bind(sort_by)
        .bind(sort_dir)
        .bind(limit)
        .bind(offset)
        .fetch_all(executor)
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        status: Option<&str>,
        statuses: &[String],
        part_id: Option<i64>,
        part_ids_in: &[i64],
        outsource_company_id: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_outsource_quote \
             WHERE deleted_at IS NULL \
               AND ($1::text IS NULL OR status = $1) \
               AND (cardinality($2::text[]) = 0 OR status = ANY($2)) \
               AND ($3::bigint IS NULL OR part_id = $3) \
               AND (cardinality($4::bigint[]) = 0 OR part_id = ANY($4)) \
               AND ($5::bigint IS NULL OR outsource_company_id = $5)",
        )
        .bind(status)
        .bind(statuses)
        .bind(part_id)
        .bind(part_ids_in)
        .bind(outsource_company_id)
        .fetch_one(executor)
        .await?;
        Ok(n)
    }

    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        new: NewOutsourceQuote,
    ) -> Result<TOutsourceQuote, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceQuote>(
            "INSERT INTO t_outsource_quote \
             (id, part_id, outsource_company_id, process_id, price, note, status, \
              created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, 'DRAFT', $7, $7) \
             RETURNING id, part_id, outsource_company_id, process_id, price, note, status, \
                       submitted_at, reviewed_at, review_note, version, \
                       created_at, created_by, updated_at, updated_by, deleted_at",
        )
        .bind(new.id)
        .bind(new.part_id)
        .bind(new.outsource_company_id)
        .bind(new.process_id)
        .bind(new.price)
        .bind(new.note)
        .bind(new.created_by)
        .fetch_one(executor)
        .await
    }

    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        price: Option<rust_decimal::Decimal>,
        note: Option<Option<&str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_note = note.is_some();
        let new_note = note.flatten();
        let r = sqlx::query(
            "UPDATE t_outsource_quote SET \
             price      = COALESCE($3::numeric, price), \
             note       = CASE WHEN $4::bool THEN $5::varchar ELSE note END, \
             version    = version + 1, \
             updated_at = now(), \
             updated_by = $6 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(version)
        .bind(price)
        .bind(set_note)
        .bind(new_note)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 提交：DRAFT → SUBMITTED，写 submitted_at。
    pub async fn submit<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_quote SET status = 'SUBMITTED', submitted_at = now(), \
             version = version + 1, updated_at = now(), updated_by = $3 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL AND status = 'DRAFT'",
        )
        .bind(id)
        .bind(version)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 审批通过：SUBMITTED → APPROVED，写 reviewed_at + review_note。
    pub async fn approve<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        review_note: Option<&str>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_quote SET status = 'APPROVED', reviewed_at = now(), \
             review_note = $3, version = version + 1, updated_at = now(), updated_by = $4 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL AND status = 'SUBMITTED'",
        )
        .bind(id)
        .bind(version)
        .bind(review_note)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 拒绝：SUBMITTED → REJECTED，必填 review_note。
    pub async fn reject<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        review_note: &str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_quote SET status = 'REJECTED', reviewed_at = now(), \
             review_note = $3, version = version + 1, updated_at = now(), updated_by = $4 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL AND status = 'SUBMITTED'",
        )
        .bind(id)
        .bind(version)
        .bind(review_note)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 把同 (part_id, process_id) 的 SUBMITTED/APPROVED 报价批量置 REJECTED（被新批准报价取代）。
    pub async fn reject_competitors<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        process_id: i64,
        exclude_id: i64,
        review_note: &str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_quote SET status = 'REJECTED', reviewed_at = now(), \
             review_note = $4, version = version + 1, updated_at = now(), updated_by = $5 \
             WHERE deleted_at IS NULL AND part_id = $1 AND process_id = $2 \
               AND id <> $3 AND status IN ('SUBMITTED', 'APPROVED')",
        )
        .bind(part_id)
        .bind(process_id)
        .bind(exclude_id)
        .bind(review_note)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_quote SET deleted_at = now(), version = version + 1, \
             updated_at = now(), updated_by = $3 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL \
               AND status IN ('DRAFT', 'REJECTED')",
        )
        .bind(id)
        .bind(version)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }
}

// ===========================================================================
// QuoteEvent (append-only)
// ===========================================================================

pub struct OutsourceQuoteEventRepo;

impl OutsourceQuoteEventRepo {
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        new: NewOutsourceQuoteEvent,
    ) -> Result<TOutsourceQuoteEvent, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceQuoteEvent>(
            "INSERT INTO t_outsource_quote_event \
             (id, quote_id, event_type, from_status, to_status, note, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING id, quote_id, event_type, from_status, to_status, note, created_by, created_at",
        )
        .bind(new.id)
        .bind(new.quote_id)
        .bind(&new.event_type)
        .bind(&new.from_status)
        .bind(&new.to_status)
        .bind(&new.note)
        .bind(new.created_by)
        .fetch_one(executor)
        .await
    }
}

// ===========================================================================
// Shipment
// ===========================================================================

pub struct OutsourceShipmentRepo;

impl OutsourceShipmentRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceShipment>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceShipment>(
            "SELECT id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
             quantity, unit_price, status, sent_at, received_at, is_billed, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_shipment \
             WHERE id = $1 AND ($2::bool OR deleted_at IS NULL)",
        )
        .bind(id)
        .bind(include_deleted)
        .fetch_optional(executor)
        .await
    }

    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        new: NewOutsourceShipment,
    ) -> Result<TOutsourceShipment, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceShipment>(
            "INSERT INTO t_outsource_shipment \
             (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
              quantity, unit_price, status, sent_at, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'OUTSOURCING', now(), $9, $9) \
             RETURNING id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
                       quantity, unit_price, status, sent_at, received_at, is_billed, version, \
                       created_at, created_by, updated_at, updated_by, deleted_at",
        )
        .bind(new.id)
        .bind(new.quote_id)
        .bind(new.part_id)
        .bind(new.batch_id)
        .bind(new.outsource_company_id)
        .bind(new.process_id)
        .bind(new.quantity)
        .bind(new.unit_price)
        .bind(new.created_by)
        .fetch_one(executor)
        .await
    }

    /// 找批次当前开口（OUTSOURCING）shipment。
    pub async fn find_open_for_batch<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
    ) -> Result<Option<TOutsourceShipment>, sqlx::Error> {
        sqlx::query_as::<_, TOutsourceShipment>(
            "SELECT id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
             quantity, unit_price, status, sent_at, received_at, is_billed, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_outsource_shipment \
             WHERE batch_id = $1 AND deleted_at IS NULL AND status = 'OUTSOURCING'",
        )
        .bind(batch_id)
        .fetch_optional(executor)
        .await
    }

    /// 标记 RECEIVED + 写 received_at。
    pub async fn mark_received<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_shipment SET status = 'RECEIVED', received_at = now(), \
             version = version + 1, updated_at = now(), updated_by = $3 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL AND status = 'OUTSOURCING'",
        )
        .bind(id)
        .bind(version)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 对账页更新：unit_price / quantity / is_billed（带 OCC）。
    pub async fn reconcile_update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        unit_price: Option<rust_decimal::Decimal>,
        quantity: Option<i32>,
        is_billed: Option<bool>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE t_outsource_shipment SET \
             unit_price  = COALESCE($3::numeric, unit_price), \
             quantity    = COALESCE($4::int, quantity), \
             is_billed   = COALESCE($5::bool, is_billed), \
             version     = version + 1, \
             updated_at  = now(), \
             updated_by  = $6 \
             WHERE id = $1 AND version = $2 AND deleted_at IS NULL \
               AND status IN ('OUTSOURCING', 'RECEIVED')",
        )
        .bind(id)
        .bind(version)
        .bind(unit_price)
        .bind(quantity)
        .bind(is_billed)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 对账页 count（行=shipment，WHERE 与 `list_for_company` 逐条一致）。
    ///
    /// 2026-10-03：原名 `count_reconciliation_for_company`，是零调用方的孤儿
    /// （前端子页从未落地）。`GET /outsource-companies/{id}/sent-parts` 补齐时
    /// 收编为正式 count，改名 `count_for_company` 与 `list_for_company` 对齐。
    pub async fn count_for_company<'e, E: PgExecutor<'e>>(
        executor: E,
        company_id: i64,
        part_ids_in: &[i64],
        sent_from: Option<chrono::NaiveDateTime>,
        sent_to: Option<chrono::NaiveDateTime>,
        received_from: Option<chrono::NaiveDateTime>,
        received_to: Option<chrono::NaiveDateTime>,
    ) -> Result<i64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_outsource_shipment \
             WHERE deleted_at IS NULL AND status IN ('OUTSOURCING', 'RECEIVED') \
               AND outsource_company_id = $1 \
               AND (cardinality($2::bigint[]) = 0 OR part_id = ANY($2)) \
               AND ($3::timestamp IS NULL OR sent_at >= $3) \
               AND ($4::timestamp IS NULL OR sent_at <= $4) \
               AND ($5::timestamp IS NULL OR received_at >= $5) \
               AND ($6::timestamp IS NULL OR received_at <= $6)",
        )
        .bind(company_id)
        .bind(part_ids_in)
        .bind(sent_from)
        .bind(sent_to)
        .bind(received_from)
        .bind(received_to)
        .fetch_one(executor)
        .await?;
        Ok(n)
    }

    /// 2026-10-03 新增：对账页 list（行 = shipment，JOIN 补齐展示字段）。
    ///
    /// 展示字段（part 图号/名称/加急、客户路径、工序名、批次号）**一次 JOIN 拿完**，
    /// service 层不再逐行回查（防 N+1）。
    ///
    /// `sort_by` / `sort_dir` 以**归一化后的白名单 token** 走 bind（`$7` / `$8`），
    /// 用 CASE 表达式选列 —— 用户输入永远不进 SQL 文本。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_for_company<'e, E: PgExecutor<'e>>(
        executor: E,
        company_id: i64,
        part_ids_in: &[i64],
        sent_from: Option<chrono::NaiveDateTime>,
        sent_to: Option<chrono::NaiveDateTime>,
        received_from: Option<chrono::NaiveDateTime>,
        received_to: Option<chrono::NaiveDateTime>,
        sort_by: &str,
        sort_dir: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceSentPartRow>, sqlx::Error> {
        sqlx::query_as::<_, OutsourceSentPartRow>(
            "SELECT s.id, s.version, s.quote_id, s.part_id, \
                    p.drawing_no, p.name, p.is_urgent, \
                    c.name AS customer_name, cp.name AS parent_customer_name, \
                    s.process_id, pr.name AS process_name, \
                    pb.batch_no, \
                    s.quantity, s.unit_price::text, s.sent_at, s.received_at, \
                    s.status, s.is_billed \
             FROM t_outsource_shipment s \
             LEFT JOIN t_part p ON p.id = s.part_id \
             LEFT JOIN t_process pr ON pr.id = s.process_id \
             LEFT JOIN t_part_batch pb ON pb.id = s.batch_id \
             LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL \
             LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL \
             WHERE s.deleted_at IS NULL AND s.status IN ('OUTSOURCING', 'RECEIVED') \
               AND s.outsource_company_id = $1 \
               AND (cardinality($2::bigint[]) = 0 OR s.part_id = ANY($2)) \
               AND ($3::timestamp IS NULL OR s.sent_at >= $3) \
               AND ($4::timestamp IS NULL OR s.sent_at <= $4) \
               AND ($5::timestamp IS NULL OR s.received_at >= $5) \
               AND ($6::timestamp IS NULL OR s.received_at <= $6) \
             ORDER BY \
               CASE WHEN $7::text = 'PRICE' AND $8::text = 'ASC' THEN s.unit_price END ASC NULLS LAST, \
               CASE WHEN $7::text = 'PRICE' AND $8::text <> 'ASC' THEN s.unit_price END DESC NULLS LAST, \
               CASE WHEN $7::text = 'RECEIVED_AT' AND $8::text = 'ASC' THEN s.received_at END ASC NULLS LAST, \
               CASE WHEN $7::text = 'RECEIVED_AT' AND $8::text <> 'ASC' THEN s.received_at END DESC NULLS LAST, \
               CASE WHEN $7::text = 'SENT_AT' AND $8::text = 'ASC' THEN s.sent_at END ASC, \
               CASE WHEN $7::text = 'SENT_AT' AND $8::text <> 'ASC' THEN s.sent_at END DESC, \
               s.id DESC \
             LIMIT $9 OFFSET $10",
        )
        .bind(company_id)
        .bind(part_ids_in)
        .bind(sent_from)
        .bind(sent_to)
        .bind(received_from)
        .bind(received_to)
        .bind(sort_by)
        .bind(sort_dir)
        .bind(limit)
        .bind(offset)
        .fetch_all(executor)
        .await
    }

    /// 2026-10-03 新增：外协在途批次 list（`status='OUTSOURCING'`）。
    ///
    /// 驱动表是 `t_outsource_shipment`，但 INNER JOIN `t_part_batch` ⋈ `t_part` ——
    /// 批次 / 零件已软删的行不再出现在在途列表（`version` / `quantity` 必须取自
    /// 批次行，缺批次时该语义无从谈起）。
    pub async fn list_in_flight<'e, E: PgExecutor<'e>>(
        executor: E,
        keyword_pat: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceInFlightRow>, sqlx::Error> {
        sqlx::query_as::<_, OutsourceInFlightRow>(
            "SELECT p.id, pb.id AS batch_id, pb.batch_no, pb.quantity, pb.version, \
                    p.serial_no, p.drawing_no, p.name, p.is_urgent, \
                    c.name AS customer_name, cp.name AS parent_customer_name, \
                    s.process_id, pr.name AS process_name, \
                    s.outsource_company_id, oc.name AS outsource_company_name, s.sent_at \
             FROM t_outsource_shipment s \
             JOIN t_part_batch pb ON pb.id = s.batch_id AND pb.deleted_at IS NULL \
             JOIN t_part p ON p.id = s.part_id AND p.deleted_at IS NULL \
             LEFT JOIN t_outsource_company oc ON oc.id = s.outsource_company_id AND oc.deleted_at IS NULL \
             LEFT JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL \
             LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL \
             LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL \
             WHERE s.deleted_at IS NULL AND s.status = 'OUTSOURCING' \
               AND ($1::text IS NULL OR p.drawing_no ILIKE $1 OR p.name ILIKE $1) \
             ORDER BY s.sent_at DESC, pb.id DESC \
             LIMIT $2 OFFSET $3",
        )
        .bind(keyword_pat)
        .bind(limit)
        .bind(offset)
        .fetch_all(executor)
        .await
    }

    pub async fn count_in_flight<'e, E: PgExecutor<'e>>(
        executor: E,
        keyword_pat: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint \
             FROM t_outsource_shipment s \
             JOIN t_part_batch pb ON pb.id = s.batch_id AND pb.deleted_at IS NULL \
             JOIN t_part p ON p.id = s.part_id AND p.deleted_at IS NULL \
             WHERE s.deleted_at IS NULL AND s.status = 'OUTSOURCING' \
               AND ($1::text IS NULL OR p.drawing_no ILIKE $1 OR p.name ILIKE $1)",
        )
        .bind(keyword_pat)
        .fetch_one(executor)
        .await?;
        Ok(n)
    }
}

// ===========================================================================
// Quotable（报价 picker：还没下发的零件，一零件一行）
// ===========================================================================

/// 2026-10-03 新增：`GET /outsource-quotes/quotable-parts` 的 SQL 真源。
///
/// ## 行粒度 = 一零件一行
/// 2026-10-03 简化前是 `(part_id, process_id)` 组合（`DISTINCT ON (p.id, pr.id)`），
/// 那套形状要求「按货架上绑了哪些 OUTSOURCE 工序」来枚举，于是被迫带出
/// `shelf_id` / `shelf_code` / `next_process_id` / `next_process_name` 四个字段。
/// 业务上「报价是给**还没下发的零件**提前锁价」，故谓词收成「该零件有 PENDING
/// 批次」：行粒度变成一零件一行，那 4 个字段全部消失（可选项的工序列表由前端从
/// `category = 'OUTSOURCE'` 的工序表自行筛选）。
///
/// **不再与 `/outsource-sendable` 同源**：两个端点的谓词已完全不同（picker 筛
/// PENDING 零件；sendable 按 `t_part_batch.current_process_id` 判「停在哪道外协
/// 工序上」），没有可共享的谓词可抽。`requires_approval` 也**不**参与本查询 ——
/// 提前锁价与该工序是否需要审批无关。
///
/// ## 筛选
/// 1. `p.deleted_at IS NULL`；
/// 2. 存在 PENDING 批次（`pb.deleted_at IS NULL AND pb.status = 'PENDING'`）——
///    同一零件多个 PENDING 批次由 `DISTINCT ON (p.id)` + `pb.batch_no ASC` 收敛为
///    一行（内层 `ORDER BY` 必须以 DISTINCT ON 的键打头，故需要这层子查询）；
/// 3. keyword（停在外层之前即内层，与 list 一致）。
pub struct OutsourceQuotableRepo;

impl OutsourceQuotableRepo {
    pub async fn list<'e, E: PgExecutor<'e>>(
        executor: E,
        keyword_pat: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceQuotableRow>, sqlx::Error> {
        sqlx::query_as::<_, OutsourceQuotableRow>(
            "SELECT q.id, q.serial_no, q.drawing_no, q.name, q.is_urgent, q.unit_price, \
                    q.customer_id, q.customer_name, q.parent_customer_name \
             FROM ( \
               SELECT DISTINCT ON (p.id) \
                  p.id, p.serial_no, p.drawing_no, p.name, p.is_urgent, p.unit_price::text AS unit_price, \
                  p.customer_id, c.name AS customer_name, cp.name AS parent_customer_name, \
                  p.is_urgent AS order_is_urgent, p.planned_delivery_date AS order_planned_date \
               FROM t_part p \
               JOIN t_part_batch pb ON pb.part_id = p.id AND pb.deleted_at IS NULL \
                 AND pb.status = 'PENDING' \
               LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL \
               LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL \
               WHERE p.deleted_at IS NULL \
                 AND ($1::text IS NULL OR p.drawing_no ILIKE $1 OR p.name ILIKE $1) \
               ORDER BY p.id, pb.batch_no ASC \
             ) q \
             ORDER BY q.order_is_urgent DESC, q.order_planned_date ASC NULLS LAST, q.id ASC \
             LIMIT $2 OFFSET $3",
        )
        .bind(keyword_pat)
        .bind(limit)
        .bind(offset)
        .fetch_all(executor)
        .await
    }

    pub async fn count<'e, E: PgExecutor<'e>>(
        executor: E,
        keyword_pat: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        // 口径必须与 list 一致（含 DISTINCT ON 收敛后的行粒度），否则分页 total 对不上。
        // keyword 过滤与 list 统一停在内层。
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM ( \
               SELECT DISTINCT p.id \
               FROM t_part p \
               JOIN t_part_batch pb ON pb.part_id = p.id AND pb.deleted_at IS NULL \
                 AND pb.status = 'PENDING' \
               WHERE p.deleted_at IS NULL \
                 AND ($1::text IS NULL OR p.drawing_no ILIKE $1 OR p.name ILIKE $1) \
             ) d",
        )
        .bind(keyword_pat)
        .fetch_one(executor)
        .await?;
        Ok(n)
    }
}

// ===========================================================================
// Sendable（可发送外协的 活跃批次 × OUTSOURCE 工序 组合）
// ===========================================================================

// ---------------------------------------------------------------------------
// 可发送外协核心 SQL（**唯一真源**，2026-10-03 抽出）
//
// 背景：`GET /outsource-sendable`（分页 list + count）与
// `GET /outsource-pool/{process_id}`（按工序取全量 + 按工序分组计数）问的是
// **同一个集合**，只是外层过滤不同。判定谓词（批次状态三态 / OUTSOURCE 类别 /
// 审批闸门 / APPROVED 报价 LEFT JOIN）在每个查询里各写一份的话，任一改动漏改
// 一处，前端就会看到「看板 tab 徽标数与 tab 内实际行数对不上」。
//
// 故把「产出行」的部分抽成常量 + 参数化投影：
// - `SENDABLE_INNER_X_SQL`：JOIN 与 WHERE（**谓词只有这一个落点**）
// - `SENDABLE_DISTINCT_D_SQL`：`DISTINCT ON (batch_id, current_process_id)` 收敛
// - `SENDABLE_OUTER_COLS` / `SENDABLE_DISPLAY_ORDER`：外层列清单 / 展示序
// - `SENDABLE_PROJECTION_*` / `SENDABLE_DEDUP_PROJECTION_*`：投影列表
//   （list / list_by_process 用全投影，count / group_sendable_counts 用精简投影）
// 调用方（`list` / `count` / `list_by_process` / `group_sendable_counts`）只换投影
// 与外层过滤，谓词一行都不重复。
//
// `DISTINCT ON` 不要求分组键出现在投影里（与 `SELECT DISTINCT` 不同），
// 故 count 用精简投影（只要分组键 + 过滤列）不违反任何约束。
// ---------------------------------------------------------------------------

/// 可发送外协查询的内层 `x`：一行 = 一个 `(batch, approved_quote)` 组合。
///
/// `{projection}` 由调用方填（全投影 / count 精简投影）；**JOIN 与 WHERE
/// 只有这一份**，四个查询共用。
///
/// ## 判定谓词 = 「批次当前停在某道外协工序上」
/// 工序来源是 `pb.current_process_id`（`JOIN t_process pr ON pr.id =
/// pb.current_process_id AND pr.category = 'OUTSOURCE'`）—— 那一列是**工序候选池
/// 归属的权威依据**（写入不变式见 `prod::batch::service::guard.rs`）。
///
/// 2026-10-03 移除的两层 JOIN：原实现要求「该 OUTSOURCE 工序同时出现在零件的
/// `process_chain_id` 链内」（`JOIN t_process_chain_step pcs`），而生产库里 1874 个
/// 零件只有 2 个绑了链、`t_process_chain_step` 里 OUTSOURCE 类的 step 有 0 条 ⇒
/// 交集恒空 ⇒ 端点恒返回空列表。同一类 bug 在 `prod::worker_pool` 的候选池 SQL 上
/// 已于 2026-09-30 以同样方式修过（全仓已无 INNER JOIN `t_process_chain_step` 残留）。
/// 业务决策：**兼容没有工序链的旧零件**，`current_process_id` 指外协工序即可发。
///
/// ## 审批闸门（`t_process.requires_approval`）
/// 该列此前是**只写不读的死字段**（process CRUD 在维护、outsource 域从未读）。本查询
/// 是第一次真正使用它：
/// - `requires_approval = false` → 免审批直发，直接出行；
/// - `requires_approval = true` → 必须已有该 (part, process) 的**真实审批**报价，
///   否则**不出行**（这是本次新增的排除语义，回归测试
///   `tests/outsource/sendable.rs::sendable_requires_approval_without_quote_excluded`）。
///
/// 报价的 LEFT JOIN 条件里带 `AND pr.requires_approval`：它把「命中报价」严格定义成
/// 「send_mode = APPROVAL」。否则免审批工序上恰好存在一条历史 APPROVED 报价时，该行
/// 会被判成 APPROVAL（报价三件套有值）而 `company_options` 又被 CASE 短路成 `[]` ——
/// 两种模式的字段契约同时被破坏。DIRECT 行的 `quote_id` / `price` / 公司三件套因此
/// 恒为 `null`，与 VO 声明一致。
///
/// ## 2026-10-03 review 第 1 轮：`AND is_direct = false`（两处谓词都要带）
/// 「APPROVED 报价」不等于「被人审批过的报价」：`prod::batch::send_to_outsource` 的
/// DIRECT 直发路径会自动建 `status='APPROVED' AND is_direct=true AND price=0` 的
/// 占位报价（见 `prod::batch::service::outsource::resolve_direct_quote_id`）。只判
/// `status='APPROVED' AND deleted_at IS NULL` 时，那个组合可达：某 (part, process)
/// 历史上被 `direct=true` 发过一次（库里留下 0 元占位报价）→ 此后该 (part, process)
/// 的批次在本列表被判成 `send_mode=APPROVAL` / `price="0.00"` /
/// `company_options=[]` —— 用户以为在按审批价发货，实际用的是一条从未被人审批过的
/// 0 元占位报价，shipment 单价落 0。故 LEFT JOIN 的 `q` 与 WHERE 里 EXISTS 的 `q2`
/// **都**要加 `AND is_direct = false`，这正是「真实审批报价」的定义。
///
/// 附带收益：DB 上有 partial unique index
/// `uq_t_outsource_quote_approved_part_process (part_id, process_id)
///  WHERE deleted_at IS NULL AND status='APPROVED' AND is_direct=false` ——
/// 加上该条件后 EXISTS 子查询的谓词与该索引谓词**逐字相等**，能直接吃这个索引；
/// 不加则退到 `ix_t_outsource_quote_part_id`（少了 `is_direct` 这一维，扫描量更大）。
/// 闸门与投影各自独立成立（`EXISTS` 判「出行与否」、LEFT JOIN 判「回哪条报价」），
/// 故两处谓词重复是有意的，改一处必须同改另一处。
///
/// ## 为什么 `t_shelf` 降级成 LEFT JOIN
/// 外层投影仍要 `shelf_code`（前端看板卡片要显示批次在哪排），但 `PENDING` 批次的
/// `current_holder_id` 恒为 `NULL`（还没上架）—— 若保持 INNER JOIN，未上架的
/// PENDING 批次会整批消失。`shelf_code` 相应改为可空（VO 本来就是 `Option`）。
///
/// ## `pb.status = 'PENDING'` 这一析取项在写侧不可达（不是 bug，别按可达路径核对）
/// 写入不变式：PENDING ⇔ 出池（`clear_process_id` 把 `current_process_id` 置 NULL，
/// 见 `prod::batch::service::guard.rs::mark_batch_with_status_and_meta` 的 doc），
/// 而本查询要求 `pr.id = pb.current_process_id` ⇒ PENDING 批次恒不满足 ⇒ 正常业务流
/// 下该析取项永远不命中。它只对 **legacy 导入数据**有意义：Python 旧库恢复脚本
/// `scripts/restore_from_backup.sh` 的 `RENAME_MAP` 把旧列
/// `t_part_batch.next_process_id` 映进 `current_process_id`，可能留下「PENDING 却带着
/// current_process_id」的组合（测试 fixture 直接 INSERT 也能造出）。保留该析取项是
/// 为了让这类行仍可被看到并手工修掉，而不是让它们在列表里彻底隐身。
const SENDABLE_INNER_X_SQL: &str = "SELECT {projection} \
             FROM t_part_batch pb \
             JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
             JOIN t_process pr ON pr.id = pb.current_process_id AND pr.deleted_at IS NULL \
               AND pr.category = 'OUTSOURCE' \
             LEFT JOIN t_shelf sh ON sh.id = pb.current_holder_id AND sh.deleted_at IS NULL \
             LEFT JOIN t_outsource_quote q ON q.part_id = p.id AND q.process_id = pr.id \
               AND q.status = 'APPROVED' AND q.is_direct = false \
               AND q.deleted_at IS NULL AND pr.requires_approval \
             LEFT JOIN t_outsource_company oc ON oc.id = q.outsource_company_id \
               AND oc.deleted_at IS NULL \
             LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL \
             LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL \
             WHERE pb.deleted_at IS NULL \
               AND (pb.status = 'PENDING' \
                    OR (pb.status = 'IN_PROCESS' AND pb.location = 'PRODUCTION_SHELF')) \
               AND ( NOT pr.requires_approval \
                  OR EXISTS (SELECT 1 FROM t_outsource_quote q2 \
                             WHERE q2.part_id = p.id AND q2.process_id = pr.id \
                               AND q2.status = 'APPROVED' AND q2.is_direct = false \
                               AND q2.deleted_at IS NULL) )";

/// `x → d` 收敛层：`DISTINCT ON (batch_id, current_process_id)`。
///
/// 排序键 `x.quote_id ASC NULLS LAST` 的作用：多个 APPROVED 报价时取 `quote_id` 最小
/// 的那条。2026-10-03 review 第 1 轮起内层报价谓词带上了 `q.is_direct = false`，
/// 而 DB 的 partial unique `uq_t_outsource_quote_approved_part_process (part_id,
/// process_id) WHERE deleted_at IS NULL AND status='APPROVED' AND is_direct=false`
/// 恰好逐字覆盖这个集合 ⇒ 同一 (part, process) 的真实审批报价至多一条，撞了 → 21303。
/// 仍保留 `DISTINCT ON` 作为兜底：并发审批 / 历史数据 / 索引缺失都可能让重复行出现，
/// 取最早批准的那条语义是「先批准的报价优先」且结果稳定，不随查询计划变化。
/// 2026-10-03 起内层不再有 `t_shelf_process` / `t_process_chain_step` 的重复行来源
/// （两层 JOIN 已删），重复行只剩报价这一处。
///
/// **为什么保留 `current_process_id` 这一列而不是只写 `DISTINCT ON (batch_id)`**：
/// `current_process_id` 由 `batch_id` 单值决定，两者语义等价。保留两列是为了让
/// `SENDABLE_PROJECTION_COUNT` / `SENDABLE_DEDUP_PROJECTION_COUNT` 与全投影共享同一
/// 组「分组键列名」，`SENDABLE_DEDUP_PROJECTION_*` 可以逐字对应（否则两套投影各维护
/// 不同的键集，改谓词时容易只改一半）。收敛后行粒度恒为「一批次一行」。
const SENDABLE_DISTINCT_D_SQL: &str = "SELECT DISTINCT ON (x.batch_id, x.current_process_id) \
             {projection} \
             FROM ( {inner} ) x \
             ORDER BY x.batch_id, x.current_process_id, x.quote_id ASC NULLS LAST";

/// 全投影（`OutsourceSendableRow` 的解码目标）。
const SENDABLE_PROJECTION_FULL: &str = "pb.version AS batch_version, pb.id AS batch_id, \
     pb.batch_no, pb.quantity AS batch_quantity, pb.status AS source_status, \
     p.id AS part_id, p.serial_no AS part_serial_no, \
     p.drawing_no AS part_drawing_no, p.name AS part_name, \
     to_char(p.planned_delivery_date, 'YYYY-MM-DD') AS planned_delivery_date, \
     p.is_urgent, p.customer_id, \
     c.name AS customer_name, cp.name AS parent_customer_name, \
     sh.code AS shelf_code, \
     pr.id AS current_process_id, pr.name AS current_process_name, \
     pr.requires_approval, \
     q.id AS quote_id, q.price::text AS price, \
     q.outsource_company_id, oc.name AS outsource_company_name, \
     CASE WHEN q.id IS NOT NULL THEN '[]'::jsonb ELSE COALESCE( \
       to_jsonb((SELECT array_agg( \
                 json_build_object('id', c2.id, 'name', c2.name) \
                 ORDER BY cp2.sort_order, c2.id) \
          FROM t_outsource_company_process cp2 \
          JOIN t_outsource_company c2 \
            ON c2.id = cp2.outsource_company_id \
           AND c2.is_active AND c2.deleted_at IS NULL \
          WHERE cp2.process_id = pr.id AND cp2.deleted_at IS NULL)), \
       '[]'::jsonb) END AS company_options";

/// 收敛层全投影（逐字对应 `SENDABLE_PROJECTION_FULL`）。
const SENDABLE_DEDUP_PROJECTION_FULL: &str = "x.batch_version, x.batch_id, x.batch_no, \
     x.batch_quantity, x.source_status, \
     x.part_id, x.part_serial_no, x.part_drawing_no, x.part_name, \
     x.planned_delivery_date, x.is_urgent, \
     x.customer_name, x.parent_customer_name, x.customer_id, \
     x.shelf_code, x.current_process_id, x.current_process_name, x.requires_approval, \
     x.quote_id, x.price, x.outsource_company_id, x.outsource_company_name, \
     x.company_options";

/// 计数侧精简投影：只要分组键 `(batch_id, current_process_id)` + 外层过滤要用的
/// 3 列（`part_drawing_no` / `part_name` / `customer_id`）+ 收敛排序键
/// `quote_id`。
///
/// 语义等价前提：分组键内 part 列恒定（同一批次恒同一零件，故这些列组内不变）。
const SENDABLE_PROJECTION_COUNT: &str = "pb.id AS batch_id, \
     p.drawing_no AS part_drawing_no, p.name AS part_name, p.customer_id, \
     pr.id AS current_process_id, q.id AS quote_id";

const SENDABLE_DEDUP_PROJECTION_COUNT: &str = "x.batch_id, x.current_process_id, \
     x.part_drawing_no, x.part_name, x.customer_id";

/// 外层列清单（`list` / `list_by_process` 共用；两者的行结构完全相同）。
const SENDABLE_OUTER_COLS: &str = "d.batch_version, d.batch_id, d.batch_no, \
     d.batch_quantity, d.source_status, \
     d.part_id, d.part_serial_no, d.part_drawing_no, d.part_name, \
     d.planned_delivery_date, d.is_urgent, \
     d.customer_name, d.parent_customer_name, \
     d.shelf_code, d.current_process_id, d.current_process_name, d.requires_approval, \
     d.quote_id, d.price, d.outsource_company_id, d.outsource_company_name, \
     d.company_options, d.customer_id";

/// 展示序（`list` / `list_by_process` 共用，保证两个端点的行序一致 —— 前端看板
/// 按 tab 渲染时可与 sendable 一览对账）。
const SENDABLE_DISPLAY_ORDER: &str = "ORDER BY d.is_urgent DESC, \
     d.planned_delivery_date ASC NULLS LAST, \
     d.part_id ASC, d.batch_no ASC, d.current_process_id ASC";

/// 拼出 `x → d` 两层子查询（供外层复用）。
///
/// ⚠️ **注入面为 0**：本函数只把调用方给的**编译期常量**（`SENDABLE_PROJECTION_*`
/// / `SENDABLE_DEDUP_PROJECTION_*`）填进 `{projection}` / `{inner}` 占位符，
/// 全程不接触任何用户输入；用户输入（keyword / customer_id / process_id）一律走
/// bind（`$1` / `$2`）。故下述 4 个查询用 `AssertSqlSafe(sql)` 包裹动态 SQL
/// 文本是安全的（口径同 `com::union_list::repo::sql`）。
fn sendable_dedup_sql(inner_projection: &str, dedup_projection: &str) -> String {
    let inner = SENDABLE_INNER_X_SQL.replace("{projection}", inner_projection);
    SENDABLE_DISTINCT_D_SQL
        .replace("{projection}", dedup_projection)
        .replace("{inner}", &inner)
}

/// 2026-10-03 新增：`GET /outsource-sendable` + `GET /outsource-pool/{process_id}`
/// 共用的 SQL 真源。
///
/// ## 行粒度 = 一批次一行
/// 三层结构：
/// 1. 内层 `x`（`SENDABLE_INNER_X_SQL`）：批次 ⋈ 零件 ⋈ OUTSOURCE 工序
///    （`pb.current_process_id`），并 LEFT JOIN 该 (part, process) 的 APPROVED
///    报价（仅限 `requires_approval = true` 的工序）；`company_options` 用标量子查询
///    `array_agg(json_build_object(...))` **一次拿完**（防 N+1）。
/// 2. 中层 `d`（`SENDABLE_DISTINCT_D_SQL`）：`DISTINCT ON` 收敛（见该常量注释）。
/// 3. 外层：各端点自己的过滤 + 展示序 + 分页。
///
/// **过滤位置约定**：keyword / customer_id / process_id 全部停在**外层 `d`** 上，
/// 内层不重复过滤 —— 谓词改动只有一个落点，不会出现「改了 list 忘了 count」。
/// 其中 `customer_id` 的谓词由共享常量 `SENDABLE_CUSTOMER_SUBTREE_PREDICATE` 提供，
/// `list` 与 `count` 引用的是同一个符号（见该常量注释）。
///
/// **DIRECT 且 `company_options` 为空的行保留返回**（前端 `canSend()` 据
/// `company_options.length >= 1` 置灰），count / counts 口径同样保留。
pub struct OutsourceSendableRepo;

/// `customer_id` 过滤谓词（2026-10-04 新增）：命中该客户**子树**，而非只命中它本身。
///
/// 零件恒挂在 L2 客户上（`t_part.customer_id` 指向 `t_customer` 的叶子行），而前端
/// 客户树选中的常常是 L1 ⇒ 只判 `d.customer_id = $2` 时，选中一个 L1 必然 total 0
/// （「可发送列表没有任何批次」的根因）。谓词分三支：
///
/// - `$2 IS NULL` → 不过滤；
/// - `d.customer_id = $2` → **该支必须保留**：传 L2 id 时行为与展开前逐字一致，
///   整个改动是纯放宽，老前端的请求参数无需任何变更即可独立上线；
/// - `d.customer_id IN (子客户)` → L1 展开一层，吃 `ix_t_customer_parent_id`。
///
/// **展开一层即完整**：客户树是严格两层结构（叶子恒 `parent_id` 指向 L1，L3 数量
/// 为 0），所以「子树」在这里就是「自身 ∪ 直接子客户」。**将来若引入 L3，必须把
/// 这条谓词改成递归 CTE**（`WITH RECURSIVE`），否则传 L1 会漏掉 L3 下的零件。
///
/// 抽成常量而非在 `list` / `count` 两处各写一遍：两处 WHERE 必须逐字一致（漏改
/// `count` 就会出现 items 与 total 对不上），共用一个符号是唯一能杜绝该漂移的写法。
const SENDABLE_CUSTOMER_SUBTREE_PREDICATE: &str = "($2::bigint IS NULL \
     OR d.customer_id = $2 \
     OR d.customer_id IN (SELECT c2.id FROM t_customer c2 \
                          WHERE c2.parent_id = $2 AND c2.deleted_at IS NULL))";

impl OutsourceSendableRepo {
    pub async fn list<'e, E: PgExecutor<'e>>(
        executor: E,
        keyword_pat: Option<&str>,
        customer_id: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceSendableRow>, sqlx::Error> {
        let dedup = sendable_dedup_sql(SENDABLE_PROJECTION_FULL, SENDABLE_DEDUP_PROJECTION_FULL);
        let sql = format!(
            "SELECT {SENDABLE_OUTER_COLS} FROM ( {dedup} ) d \
             WHERE ($1::text IS NULL OR d.part_drawing_no ILIKE $1 OR d.part_name ILIKE $1) \
               AND {} \
             {SENDABLE_DISPLAY_ORDER} \
             LIMIT $3 OFFSET $4",
            SENDABLE_CUSTOMER_SUBTREE_PREDICATE,
        );
        sqlx::query_as::<_, OutsourceSendableRow>(AssertSqlSafe(sql))
            .bind(keyword_pat)
            .bind(customer_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(executor)
            .await
    }

    pub async fn count<'e, E: PgExecutor<'e>>(
        executor: E,
        keyword_pat: Option<&str>,
        customer_id: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        // 与 list 的 WHERE + DISTINCT ON 口径逐条一致（含 DIRECT 空 options 行）——
        // 因为谓词来自同一份 `SENDABLE_INNER_X_SQL`，只剩投影不同。
        let dedup = sendable_dedup_sql(SENDABLE_PROJECTION_COUNT, SENDABLE_DEDUP_PROJECTION_COUNT);
        let sql = format!(
            "SELECT COUNT(*)::bigint FROM ( {dedup} ) d \
             WHERE ($1::text IS NULL OR d.part_drawing_no ILIKE $1 OR d.part_name ILIKE $1) \
               AND {}",
            SENDABLE_CUSTOMER_SUBTREE_PREDICATE,
        );
        let n: i64 = sqlx::query_scalar(AssertSqlSafe(sql))
            .bind(keyword_pat)
            .bind(customer_id)
            .fetch_one(executor)
            .await?;
        Ok(n)
    }

    /// 2026-10-03 新增：`GET /outsource-pool/{process_id}` 的候选批次 list。
    ///
    /// 与 [`list`] 的**唯一差别**是外层 `WHERE d.current_process_id = $1`（而非
    /// keyword / customer_id + 分页）。判定谓词、行粒度、排序全部来自同一批
    /// 常量，故「看板 tab 内行」与「sendable 一览按 `current_process_id` 过滤的行」
    /// 逐字段一致（`tests/outsource/pool.rs` 有专门断言守这条）。
    ///
    /// 不分页：admin 看板视角，一个 tab 要一次拿全（与
    /// `prod::pool/{process_id}` 同取舍）。
    pub async fn list_by_process<'e, E: PgExecutor<'e>>(
        executor: E,
        process_id: i64,
    ) -> Result<Vec<OutsourceSendableRow>, sqlx::Error> {
        let dedup = sendable_dedup_sql(SENDABLE_PROJECTION_FULL, SENDABLE_DEDUP_PROJECTION_FULL);
        let sql = format!(
            "SELECT {SENDABLE_OUTER_COLS} FROM ( {dedup} ) d \
             WHERE d.current_process_id = $1 \
             {SENDABLE_DISPLAY_ORDER}"
        );
        sqlx::query_as::<_, OutsourceSendableRow>(AssertSqlSafe(sql))
            .bind(process_id)
            .fetch_all(executor)
            .await
    }
}

// ===========================================================================
// Pool（`GET /outsource-pool/*`：按外协工序切 tab 的看板三端点，2026-10-03 新增）
// ===========================================================================
//
// 与 Sendable 的分工：
// - **候选侧**（`sendable_count` / `items`）复用 `OutsourceSendableRepo` 的
//   核心 SQL —— 看板左列必须与 sendable 一览同口径，不能各写一份。
// - **在途侧**（`in_flight_count` / `/state` 的 items）以 `t_part_batch` 为驱动
//   （`status='OUTSOURCE' AND location='OUTSOURCE_COMPANY'`），因为在途批次的
//   归属锚是批次自身的 `current_holder_id`（公司）+ `current_process_id`（工序），
//   写侧 `send_to_outsource` 就是这么落的。

/// 2026-10-03 新增：`GET /outsource-pool/*` 三个端点的 SQL 真源。
pub struct OutsourcePoolRepo;

impl OutsourcePoolRepo {
    /// 候选侧按工序分组计数（`GROUP BY current_process_id`）。
    ///
    /// 与 `sendable_list_by_process` 的行粒度**逐行一致**（同一份核心 SQL），
    /// 故看板 tab 徽标「可发 N」与 tab 内 `items.len()` 天然相等。
    /// 只返回 count > 0 的工序（`GROUP BY` 不输出 0 行，与
    /// `prod::pool/counts` 同取舍）。
    pub async fn group_sendable_counts<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        let dedup = sendable_dedup_sql(SENDABLE_PROJECTION_COUNT, SENDABLE_DEDUP_PROJECTION_COUNT);
        let sql = format!(
            "SELECT d.current_process_id, COUNT(*)::bigint \
             FROM ( {dedup} ) d \
             GROUP BY d.current_process_id \
             ORDER BY d.current_process_id ASC"
        );
        let rows: Vec<(i64, i64)> = sqlx::query_as(AssertSqlSafe(sql))
            .fetch_all(executor)
            .await?;
        Ok(rows)
    }

    /// 在途侧按工序分组计数（`GROUP BY current_process_id`）。
    ///
    /// 三态与 `/outsource-pool/{process_id}` 的 `companies[].held_count` 一致：
    /// `status='OUTSOURCE' AND location='OUTSOURCE_COMPANY' AND deleted_at IS NULL`。
    pub async fn group_in_flight_counts<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT pb.current_process_id, COUNT(*)::bigint \
             FROM t_part_batch pb \
             WHERE pb.status = 'OUTSOURCE' \
               AND pb.location = 'OUTSOURCE_COMPANY' \
               AND pb.deleted_at IS NULL \
               AND pb.current_process_id IS NOT NULL \
             GROUP BY pb.current_process_id \
             ORDER BY pb.current_process_id ASC",
        )
        .fetch_all(executor)
        .await?;
        Ok(rows)
    }

    /// 该工序映射的**全部活跃外协公司** + 各公司在该工序在外协的批次数。
    ///
    /// `held_count` 用 `LEFT JOIN` 出：**没有在途批次的公司也要出现在列表里**
    /// （前端看板要渲染空公司列当拖拽目标）。停用 / 软删的公司不出现。
    ///
    /// `ORDER BY MIN(cp.sort_order), c.id`：`t_outsource_company_process` 有
    /// partial unique `(outsource_company_id, process_id) WHERE deleted_at IS NULL`，
    /// 正常每公司只有一条映射行；用 `MIN()` 包一层是为了不依赖这条唯一性推断
    /// （历史脏数据下 GROUP BY 仍只出一组）。
    pub async fn list_companies_with_held<'e, E: PgExecutor<'e>>(
        executor: E,
        process_id: i64,
    ) -> Result<Vec<OutsourcePoolCompanyRow>, sqlx::Error> {
        sqlx::query_as::<_, OutsourcePoolCompanyRow>(
            "SELECT c.id AS company_id, c.name, COUNT(pb.id)::bigint AS held_count \
             FROM t_outsource_company_process cp \
             JOIN t_outsource_company c \
               ON c.id = cp.outsource_company_id \
              AND c.is_active AND c.deleted_at IS NULL \
             LEFT JOIN t_part_batch pb \
               ON pb.current_holder_id = c.id \
              AND pb.status = 'OUTSOURCE' \
              AND pb.location = 'OUTSOURCE_COMPANY' \
              AND pb.current_process_id = $1 \
              AND pb.deleted_at IS NULL \
             WHERE cp.process_id = $1 AND cp.deleted_at IS NULL \
             GROUP BY c.id, c.name \
             ORDER BY MIN(cp.sort_order) ASC, c.id ASC",
        )
        .bind(process_id)
        .fetch_all(executor)
        .await
    }

    /// 某公司在某工序在外协的全部批次（看板右列 = 公司列的卡片）。
    ///
    /// **一条 list SQL 拿完**（防 N+1）：公司名 / 工序名 / 客户路径 / shipment
    /// 字段 / 下一道工序全部在 SQL 内 JOIN / LATERAL 解析，service 层零回查。
    ///
    /// `LEFT JOIN LATERAL` 派生 `receive_next_process_*`，两步定位：
    /// 1. **锚链** = `COALESCE(p.process_chain_id, cur.chain_id)`（`cur` =
    ///    `pb.current_process_step_id` 指向的 step，只用于回退取链 id）；
    /// 2. **当前 step 在锚链内的位置**：`cur2.process_id = pb.current_process_id`；
    ///    再取锚链内 `sort_order = cur2.sort_order + 1` 的未软删 step（唯一索引
    ///    `uq_chain_step_chain_order (chain_id, sort_order) WHERE deleted_at IS NULL`
    ///    ⇒ 唯一无歧义）。中间 JOIN `t_part_process_chain` 是为了让「锚链已软删」
    ///    也落到「无下一 step」分支（`chain_resolvable=false`）。
    ///
    /// ⚠️ **第 2 步必须按 `current_process_id` 在锚链内重新定位，不能拿
    /// `pb.current_process_step_id` 的 `sort_order` 直接当位置**：step 指针与
    /// 「当前工序在链内的位置」是两个独立事实，后者漂移时按位置推进会把**外协
    /// 工序自己**当成下一道工序返回，而 `chain_resolvable` 仍在说「可免填」⇒ 写侧
    /// 照单全收，静默错值比拒收更难发现。
    ///
    /// **锚链与写侧同源**：写侧 `receive_from_outsource` 走
    /// `optional_process_chain(part_id)`（读 `t_part.process_chain_id`）+
    /// `optional_step_id(chain_id, process_id)`。锚 `p.process_chain_id` ⇒ 本端点
    /// 返回的 process_id 必然是**锚链内活跃 step 的工序**，写侧能在同一条链上解析到
    /// （有链时）。
    ///
    /// 2026-10-03 写侧放松了链的必须性（无链零件可发可收，见
    /// `prod::batch::service::guard.rs::optional_process_chain`），对本查询的影响：
    ///
    /// - **无链批次**（`p.process_chain_id IS NULL`）落到 `chain_resolvable = false`
    ///   分支：派生值 `COALESCE(..., 0) = 0`，前端据此弹「需手填下一道工序」对话框。
    ///   这条降级路径本轮之前就已实现，不是新增风险。
    /// - **有链但链内找不到该工序**的批次读侧同样给 `chain_resolvable = false`，而写侧
    ///   `optional_step_id` 会以 `20702 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND` 拒收。
    ///   两侧的处置方向一致（都让用户手填），但**读侧不会替写侧报 20702** ——
    ///   前端读 `chain_resolvable == false` 就弹手填框，不会走到服务端拒收那条路。
    ///
    /// `COALESCE(p.process_chain_id, cur.chain_id)` 回落分支**不再是「脏数据专属的
    /// 死代码」**：无链批次本身就是活场景（2026-10-03 起它们能发外协，也就可能在途），
    /// 每次派生都会走到这个回落分支。但它**取到的仍是 NULL** —— 无链批次的
    /// `current_process_step_id` 按新的写入不变式恒为 NULL（`optional_step_id` 在无链
    /// 时返回 `None`），`cur` 子查询无行 ⇒ 锚链解析失败 ⇒ 落 `chain_resolvable =
    /// false`。保留 `COALESCE` 仍是对的（读侧不假设写侧何时写 step），它现在守的是
    /// 「无链」这一常态，而不是「链被软删 / 数据坏了」。
    ///
    /// 2026-10-03 登记**保留而非删掉**该回落分支的取舍：不写清楚理由就会被后人当
    /// 垃圾清理。删掉后无链批次与链尾批次会落到同一个「无下一 step」分支，两者的
    /// 派生结果本就相同（都是 NULL），所以删掉不改变任何返回值；保留的成本是一个
    /// `COALESCE`，收益是读侧 SQL 自己说明了「锚链优先取 part 的，缺了才回退到
    /// step 指针所在的链」这一意图。
    ///
    /// ⚠️ **两个派生列都必须显式 `AS receive_next_process_*`**：LATERAL 子查询的输出
    /// 列名只跟子查询内部的名字走（`nx.next_process_name` 的列名是
    /// `next_process_name`，不带 `nx.` 前缀），不写别名时 runtime `query_as` 的
    /// `FromRow` 会报 `ColumnNotFound("receive_next_process_name")`。末尾
    /// `LIMIT 1` 保证 LATERAL 恒至多一行：锚链内同一 `process_id` 重复属数据异常，
    /// 而这不是读侧单方面放过的缺口 —— 写侧 `resolve_step_id_by_process`
    /// （`src/modules/prod/process_chain/repo/query.rs`）自己登记的立场逐字是
    /// 「链内同一 process_id 重复（数据异常）的歧义不在本函数守，caller 用
    /// `count_steps_by_chain_process` 单独查证」，读侧沿用同一口径收口、不另立
    /// 一套；不设 `LIMIT` 会把一行批次扇成多行、破坏 VO 层
    /// 「`current_held == items.len()`」。
    ///
    /// `t_applicant` 走 `LEFT JOIN LATERAL (… ORDER BY ap.id ASC LIMIT 1)` 而不是
    /// 直接 JOIN：`t_part.applicant_name` 是字符串非 FK，而 `t_applicant` 的唯一索引
    /// 是 `(name, customer_id)`，**name 单独不唯一** —— 同名申请人跨客户存在时直接
    /// JOIN 会把一行批次扇出成多行，破坏上面那个 `current_held == items.len()`
    /// 不变量（`batch_id` 重复 + 计数虚高）。投影只有 `ap.name` 一个列，而
    /// `ap.name = p.applicant_name` 由 WHERE 保证恒等 ⇒ **取哪一行取值都一样**，
    /// `ORDER BY ap.id ASC` 的作用只是给这条 LATERAL 一个确定的行（配合
    /// `LIMIT 1`），零语义内容；不要把它当成「挑了某个申请人」。
    pub async fn list_held<'e, E: PgExecutor<'e>>(
        executor: E,
        company_id: i64,
        process_id: i64,
    ) -> Result<Vec<OutsourceHeldBatchRow>, sqlx::Error> {
        sqlx::query_as::<_, OutsourceHeldBatchRow>(
            "SELECT pb.id AS batch_id, pb.part_id, pb.batch_no, pb.quantity, \
                    p.serial_no, p.drawing_no, p.name, \
                    p.system_delivery_date, p.planned_delivery_date, p.is_urgent, \
                    c2.name AS customer_name, c1.name AS parent_customer_name, \
                    a.name AS applicant_name, \
                    pb.location AS batch_location, p.note, \
                    pb.version AS batch_version, \
                    s.sent_at, s.unit_price::text AS price, \
                    COALESCE(nx.next_process_id, 0) AS receive_next_process_id, \
                    nx.next_process_name AS receive_next_process_name \
             FROM t_part_batch pb \
             JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
             LEFT JOIN t_customer c2 ON c2.id = p.customer_id AND c2.deleted_at IS NULL \
             LEFT JOIN t_customer c1 ON c1.id = c2.parent_id AND c1.deleted_at IS NULL \
             LEFT JOIN LATERAL ( \
               SELECT ap.name \
               FROM t_applicant ap \
               WHERE ap.name = p.applicant_name AND ap.deleted_at IS NULL \
               ORDER BY ap.id ASC \
               LIMIT 1 \
             ) a ON TRUE \
             LEFT JOIN t_outsource_shipment s \
               ON s.batch_id = pb.id AND s.status = 'OUTSOURCING' AND s.deleted_at IS NULL \
             LEFT JOIN LATERAL ( \
               SELECT nsp.process_id AS next_process_id, np.name AS next_process_name \
               FROM t_process_chain_step cur \
               JOIN t_part_process_chain pc \
                 ON pc.id = COALESCE(p.process_chain_id, cur.chain_id) \
                AND pc.deleted_at IS NULL \
               JOIN t_process_chain_step cur2 \
                 ON cur2.chain_id = pc.id \
                AND cur2.process_id = pb.current_process_id \
                AND cur2.deleted_at IS NULL \
               JOIN t_process_chain_step nsp \
                 ON nsp.chain_id = pc.id \
                AND nsp.sort_order = cur2.sort_order + 1 \
                AND nsp.deleted_at IS NULL \
               LEFT JOIN t_process np \
                 ON np.id = nsp.process_id AND np.deleted_at IS NULL \
               WHERE cur.id = pb.current_process_step_id AND cur.deleted_at IS NULL \
               LIMIT 1 \
             ) nx ON TRUE \
             WHERE pb.status = 'OUTSOURCE' \
               AND pb.location = 'OUTSOURCE_COMPANY' \
               AND pb.current_holder_id = $1 \
               AND pb.current_process_id = $2 \
               AND pb.deleted_at IS NULL \
             ORDER BY pb.id ASC",
        )
        .bind(company_id)
        .bind(process_id)
        .fetch_all(executor)
        .await
    }
}
