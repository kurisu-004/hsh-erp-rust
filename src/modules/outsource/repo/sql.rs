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

use sqlx::PgExecutor;

use super::super::model::{
    NewOutsourceCompany, NewOutsourceCompanyProcess, NewOutsourceQuote, NewOutsourceQuoteEvent,
    NewOutsourceShipment, TOutsourceCompany, TOutsourceCompanyProcess, TOutsourceQuote,
    TOutsourceQuoteEvent, TOutsourceShipment,
};
use super::{
    OutsourceInFlightRow, OutsourceQuotableRow, OutsourceSendableRow, OutsourceSentPartRow,
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
// Quotable（报价 picker：可建报价的 零件 × OUTSOURCE 工序 组合）
// ===========================================================================

/// 2026-10-03 新增：`GET /outsource-quotes/quotable-parts` 的 SQL 真源。
///
/// ## 行粒度 = (part_id, process_id)
/// 用 `DISTINCT ON (p.id, pr.id)` 去重（外层 `ORDER BY` 另算展示序）。
/// 同一台零件有多个符合条件的活跃批次时，靠内层 `ORDER BY … pb.batch_no ASC`
/// 选 batch_no 最小的那条做代表行。
///
/// ## 4 层筛选（缺一不可）
/// 1. 活跃批次：PENDING，或 IN_PROCESS + PRODUCTION_SHELF；
/// 2. 批次所在货架绑了 OUTSOURCE 工序（`t_shelf_process` ⋈ `t_process`）；
/// 3. **该 OUTSOURCE 工序在该 part 的活跃工艺链里**（`t_process_chain_step`）
///    —— 少了这条会给出工艺链上不存在的工序，后续 `send-to-outsource` 的
///    `resolve_step_id_by_process` 会 404；
/// 4. `t_part` 未软删。
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
                    q.customer_id, q.customer_name, q.parent_customer_name, \
                    q.shelf_id, q.shelf_code, q.next_process_id, q.next_process_name \
             FROM ( \
               SELECT DISTINCT ON (p.id, pr.id) \
                 p.id, p.serial_no, p.drawing_no, p.name, p.is_urgent, p.unit_price::text AS unit_price, \
                 p.customer_id, c.name AS customer_name, cp.name AS parent_customer_name, \
                 sh.id AS shelf_id, sh.code AS shelf_code, \
                 pr.id AS next_process_id, pr.name AS next_process_name, \
                 p.is_urgent AS order_is_urgent, p.planned_delivery_date AS order_planned_date, \
                 pb.batch_no AS order_batch_no \
               FROM t_part_batch pb \
               JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
               JOIN t_shelf sh ON sh.id = pb.current_holder_id AND sh.deleted_at IS NULL \
               JOIN t_shelf_process sp ON sp.shelf_id = sh.id AND sp.deleted_at IS NULL \
               JOIN t_process pr ON pr.id = sp.process_id AND pr.deleted_at IS NULL \
                 AND pr.category = 'OUTSOURCE' \
               JOIN t_process_chain_step pcs ON pcs.chain_id = p.process_chain_id \
                 AND pcs.process_id = pr.id AND pcs.deleted_at IS NULL \
               LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL \
               LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL \
               WHERE pb.deleted_at IS NULL \
                 AND (pb.status = 'PENDING' \
                      OR (pb.status = 'IN_PROCESS' AND pb.location = 'PRODUCTION_SHELF')) \
               ORDER BY p.id, pr.id, pb.batch_no ASC \
             ) q \
             WHERE ($1::text IS NULL OR q.drawing_no ILIKE $1 OR q.name ILIKE $1) \
             ORDER BY q.order_is_urgent DESC, q.order_planned_date ASC NULLS LAST, \
                      q.id ASC, q.next_process_id ASC \
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
        // 口径必须与 list 一致（含 DISTINCT ON 去重后的行粒度），否则分页 total 对不上。
        // 2026-10-03 review 第 1 轮 m3：keyword 过滤与 list 统一停在外层 `d`
        // （此前 count 下推到最内层、`list` 停在外层，两侧漂移）。
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM ( \
               SELECT DISTINCT p.id, pr.id, p.drawing_no, p.name \
               FROM t_part_batch pb \
               JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
               JOIN t_shelf sh ON sh.id = pb.current_holder_id AND sh.deleted_at IS NULL \
               JOIN t_shelf_process sp ON sp.shelf_id = sh.id AND sp.deleted_at IS NULL \
               JOIN t_process pr ON pr.id = sp.process_id AND pr.deleted_at IS NULL \
                 AND pr.category = 'OUTSOURCE' \
               JOIN t_process_chain_step pcs ON pcs.chain_id = p.process_chain_id \
                 AND pcs.process_id = pr.id AND pcs.deleted_at IS NULL \
               WHERE pb.deleted_at IS NULL \
                 AND (pb.status = 'PENDING' \
                      OR (pb.status = 'IN_PROCESS' AND pb.location = 'PRODUCTION_SHELF')) \
             ) d \
             WHERE ($1::text IS NULL OR d.drawing_no ILIKE $1 OR d.name ILIKE $1)",
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

/// 2026-10-03 新增：`GET /outsource-sendable` 的 SQL 真源。
///
/// ## 行粒度 = (batch_id, next_process_id)
/// 三层结构：
/// 1. 内层：批次 ⋈ 零件 ⋈ 货架 ⋈ 货架工序 ⋈ OUTSOURCE 工序 ⋈ 工艺链 step，
///    并 LEFT JOIN 该 (part, process) 的 APPROVED 报价；`company_options` 用
///    标量子查询 `array_agg(json_build_object(...))` **一次拿完**（防 N+1）。
/// 2. 中层 `DISTINCT ON (batch_id, next_process_id)`：**多个 APPROVED 报价时取
///    `quote_id ASC NULLS LAST` 的第一条**。DB 有 partial unique
///    `uq_t_outsource_quote_approved_part_process` 兜底（撞了 → 21303），但并发
///    审批 / 历史数据仍可能出现多条；此时取最早批准的那条，语义上是「先批准的
///    报价优先」且结果稳定（不会随查询计划变化）。
/// 3. 外层：keyword / customer_id 过滤 + 展示序 + LIMIT/OFFSET。
///
/// **count 的过滤位置必须与 list 一致**（2026-10-03 review 第 1 轮 m3）：两者都把
/// keyword / customer_id 停在**外层** `d` 上，内层不重复过滤、不重复投影。
/// 谓词改动只有一个落点，不会出现「改了 list 忘了 count」。
///
/// **DIRECT 且 `company_options` 为空的行保留返回**（前端 `canSend()` 据
/// `company_options.length >= 1` 置灰），count 口径同样保留。
pub struct OutsourceSendableRepo;

impl OutsourceSendableRepo {
    pub async fn list<'e, E: PgExecutor<'e>>(
        executor: E,
        keyword_pat: Option<&str>,
        customer_id: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceSendableRow>, sqlx::Error> {
        sqlx::query_as::<_, OutsourceSendableRow>(
            "SELECT d.batch_version, d.batch_id, d.batch_no, d.batch_quantity, d.source_status, \
                    d.part_id, d.part_serial_no, d.part_drawing_no, d.part_name, \
                    d.planned_delivery_date, d.is_urgent, \
                    d.customer_name, d.parent_customer_name, \
                    d.shelf_code, d.next_process_id, d.next_process_name, \
                    d.quote_id, d.price, d.outsource_company_id, d.outsource_company_name, \
                    d.company_options, d.customer_id \
             FROM ( \
               SELECT DISTINCT ON (x.batch_id, x.next_process_id) \
                 x.batch_version, x.batch_id, x.batch_no, x.batch_quantity, x.source_status, \
                 x.part_id, x.part_serial_no, x.part_drawing_no, x.part_name, \
                 x.planned_delivery_date, x.is_urgent, \
                 x.customer_name, x.parent_customer_name, x.customer_id, \
                 x.shelf_code, x.next_process_id, x.next_process_name, \
                 x.quote_id, x.price, x.outsource_company_id, x.outsource_company_name, \
                 x.company_options \
               FROM ( \
                 SELECT pb.version AS batch_version, pb.id AS batch_id, pb.batch_no, \
                        pb.quantity AS batch_quantity, pb.status AS source_status, \
                        p.id AS part_id, p.serial_no AS part_serial_no, \
                        p.drawing_no AS part_drawing_no, p.name AS part_name, \
                        to_char(p.planned_delivery_date, 'YYYY-MM-DD') AS planned_delivery_date, \
                        p.is_urgent, p.customer_id, \
                        c.name AS customer_name, cp.name AS parent_customer_name, \
                        sh.code AS shelf_code, \
                        pr.id AS next_process_id, pr.name AS next_process_name, \
                        q.id AS quote_id, q.price::text AS price, \
                        q.outsource_company_id, oc.name AS outsource_company_name, \
                        sp.id AS sp_id, pcs.id AS step_id, \
                        CASE WHEN q.id IS NOT NULL THEN '[]'::jsonb ELSE COALESCE( \
                          to_jsonb((SELECT array_agg( \
                                    json_build_object('id', c2.id, 'name', c2.name) \
                                    ORDER BY cp2.sort_order, c2.id) \
                             FROM t_outsource_company_process cp2 \
                             JOIN t_outsource_company c2 \
                               ON c2.id = cp2.outsource_company_id \
                              AND c2.is_active AND c2.deleted_at IS NULL \
                             WHERE cp2.process_id = pr.id AND cp2.deleted_at IS NULL)), \
                          '[]'::jsonb) END AS company_options \
                 FROM t_part_batch pb \
                 JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
                 JOIN t_shelf sh ON sh.id = pb.current_holder_id AND sh.deleted_at IS NULL \
                 JOIN t_shelf_process sp ON sp.shelf_id = sh.id AND sp.deleted_at IS NULL \
                 JOIN t_process pr ON pr.id = sp.process_id AND pr.deleted_at IS NULL \
                   AND pr.category = 'OUTSOURCE' \
                 JOIN t_process_chain_step pcs ON pcs.chain_id = p.process_chain_id \
                   AND pcs.process_id = pr.id AND pcs.deleted_at IS NULL \
                 LEFT JOIN t_outsource_quote q ON q.part_id = p.id AND q.process_id = pr.id \
                   AND q.status = 'APPROVED' AND q.deleted_at IS NULL \
                 LEFT JOIN t_outsource_company oc ON oc.id = q.outsource_company_id \
                   AND oc.deleted_at IS NULL \
                 LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL \
                 LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL \
                 WHERE pb.deleted_at IS NULL \
                   AND (pb.status = 'PENDING' \
                        OR (pb.status = 'IN_PROCESS' AND pb.location = 'PRODUCTION_SHELF')) \
               ) x \
               ORDER BY x.batch_id, x.next_process_id, x.sp_id, x.step_id, \
                        x.quote_id ASC NULLS LAST \
             ) d \
             WHERE ($1::text IS NULL OR d.part_drawing_no ILIKE $1 OR d.part_name ILIKE $1) \
               AND ($2::bigint IS NULL OR d.customer_id = $2) \
             ORDER BY d.is_urgent DESC, d.planned_delivery_date ASC NULLS LAST, \
                      d.part_id ASC, d.batch_no ASC, d.next_process_id ASC \
             LIMIT $3 OFFSET $4",
        )
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
        // 与 list 的 WHERE + DISTINCT ON 口径逐条一致（含 DIRECT 空 options 行）。
        // 2026-10-03 review 第 1 轮 m3：keyword / customer_id 此前被下推到**最内层**
        // （list 留在外层 WHERE），两侧过滤位置不一致 —— 今天语义等价（分组键
        // `(batch_id, next_process_id)` 内 part 列恒定），但改动时容易只改一侧。
        // 现与 list 统一停在外层，过滤谓词只剩一个落点；内层也不再需要投影
        // `part_drawing_no` / `part_name` / `customer_id`（此前是为下推的过滤备的，
        // 改到外层后是死投影）。
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM ( \
               SELECT DISTINCT ON (x.batch_id, x.next_process_id) \
                 x.batch_id, x.next_process_id, \
                 x.part_drawing_no, x.part_name, x.customer_id \
               FROM ( \
                 SELECT pb.id AS batch_id, \
                        p.drawing_no AS part_drawing_no, p.name AS part_name, p.customer_id, \
                        pr.id AS next_process_id, sp.id AS sp_id, pcs.id AS step_id, \
                        q.id AS quote_id \
                 FROM t_part_batch pb \
                 JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
                 JOIN t_shelf sh ON sh.id = pb.current_holder_id AND sh.deleted_at IS NULL \
                 JOIN t_shelf_process sp ON sp.shelf_id = sh.id AND sp.deleted_at IS NULL \
                 JOIN t_process pr ON pr.id = sp.process_id AND pr.deleted_at IS NULL \
                   AND pr.category = 'OUTSOURCE' \
                 JOIN t_process_chain_step pcs ON pcs.chain_id = p.process_chain_id \
                   AND pcs.process_id = pr.id AND pcs.deleted_at IS NULL \
                 LEFT JOIN t_outsource_quote q ON q.part_id = p.id AND q.process_id = pr.id \
                   AND q.status = 'APPROVED' AND q.deleted_at IS NULL \
                 WHERE pb.deleted_at IS NULL \
                   AND (pb.status = 'PENDING' \
                        OR (pb.status = 'IN_PROCESS' AND pb.location = 'PRODUCTION_SHELF')) \
               ) x \
               ORDER BY x.batch_id, x.next_process_id, x.sp_id, x.step_id, \
                        x.quote_id ASC NULLS LAST \
             ) d \
             WHERE ($1::text IS NULL OR d.part_drawing_no ILIKE $1 OR d.part_name ILIKE $1) \
               AND ($2::bigint IS NULL OR d.customer_id = $2)",
        )
        .bind(keyword_pat)
        .bind(customer_id)
        .fetch_one(executor)
        .await?;
        Ok(n)
    }
}
