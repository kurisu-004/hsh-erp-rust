//! outsource 域数据访问（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/repository/outsource*.py。
//!
//! 实施约定：
//! - 全部 `sqlx::query_as::<_, T>` + `FromRow`（runtime，不依赖 `.sqlx/` 缓存）
//! - 软删：默认 `deleted_at IS NULL`；写 UPDATE 带 `WHERE id=$1 AND version=$2` 乐观锁
//! - 0 行由 service 转 409 / BIZ_VERSION_CONFLICT
//! - `&mut PgConnection` 走引用 `&mut *conn`，与 part / customer 域同形

use sqlx::PgExecutor;

use super::model::{
    NewOutsourceCompany, NewOutsourceCompanyProcess, NewOutsourceQuote, NewOutsourceQuoteEvent,
    NewOutsourceShipment, TOutsourceCompany, TOutsourceCompanyProcess, TOutsourceQuote,
    TOutsourceQuoteEvent, TOutsourceShipment,
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

    /// 对账页 list（行=shipment + 左联 part / process / company）
    pub async fn count_reconciliation_for_company<'e, E: PgExecutor<'e>>(
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
}
