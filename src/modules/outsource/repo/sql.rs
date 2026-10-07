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
//!
//! 2026-10-09：`OutsourcePoolRepo`（`GET /outsource-pool/*` 三条旧读的 SQL 真源）整
//! 体删除，其 4 个方法在端点下线后全部无调用方；看板两条新读的 SQL 在
//! `../board/repo.rs`（候选侧仍复用本文件的 `SENDABLE_INNER_X_SQL` 谓词唯一落点与
//! `sendable_dedup_sql` / 两个 `*_PROJECTION_FULL` 常量）。
//! 同日 `NEXT_PROCESS_LATERAL_SQL`（「下一道工序」推导子查询）从
//! `../board/repo.rs::SQL_HELD_BY_PROCESS` 抽到这里：看板在途卡的 `receive_next_*`
//! 与移动写端点省略 `next_process_id` 时的推导问的是同一个事实，两处各写一份必然
//! 分叉（前端看板上显示「可免填」、写端点却拒收，或反之）。
//!
//! 同日 `OutsourceQuoteRepo::update` 删除（`POST /outsource-quotes/{id}/update`
//! 硬切下线，前端零消费）；报价一览与对账页的 `keyword` 预搜索（`part_keyword_search`）
//! 一并删除 —— 两处的 `keyword` 都改成 `drawing_no` / `name` 直连 ILIKE 谓词，
//! `p.drawing_no` / `p.name` 在各自的 SQL 里本来就已 SELECT ⇒ **零 JOIN 改动**，
//! 省掉的是 `LIMIT 10000` 无 `ORDER BY` 的静默截断风险与配套的「零命中早返回」守卫。

use sqlx::PgExecutor;

use super::super::model::{
    NewOutsourceCompany, NewOutsourceCompanyProcess, NewOutsourceQuote, NewOutsourceQuoteEvent,
    NewOutsourceShipment, TOutsourceCompany, TOutsourceCompanyProcess, TOutsourceQuote,
    TOutsourceQuoteEvent, TOutsourceShipment,
};
use super::{
    OutsourceInFlightRow, OutsourceQuotableRow, OutsourceSentPartFilter, OutsourceSentPartRow,
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
        drawing_no_pat: Option<&str>,
        name_pat: Option<&str>,
        is_urgent: Option<bool>,
        sort_by: &str,
        sort_dir: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TOutsourceQuote>, sqlx::Error> {
        // 单一固定 ORDER BY（避免动态 SQL 字符串）：用 case 表达式选列
        //
        // 2026-10-09：`keyword` 拆成 `drawing_no` / `name` 两个直连 ILIKE 的谓词，
        // 外加 `is_urgent` 精确谓词。为此加了一条 `LEFT JOIN t_part p` ——
        // **LEFT 而非 INNER**：不传任何零件侧筛选时它必须恒等空操作（零件已软删 /
        // part_id 悬空的存量报价仍要出现在一览里）；传了筛选时谓词 `p.col …` 在 NULL 行
        // 上求值为 NULL、不成立，行为与 INNER JOIN 一致。
        // JOIN 要求给 `t_outsource_quote` 起别名 `q`：两表都有 `id` / `deleted_at`
        // 等同名列，不加前缀的 `SELECT id` / `WHERE deleted_at IS NULL` 会被 PG 判成
        // ambiguous 而 42703。
        sqlx::query_as::<_, TOutsourceQuote>(
            "SELECT q.id, q.part_id, q.outsource_company_id, q.process_id, q.price, q.note, q.status, \
             q.submitted_at, q.reviewed_at, q.review_note, q.version, \
             q.created_at, q.created_by, q.updated_at, q.updated_by, q.deleted_at \
             FROM t_outsource_quote q \
             LEFT JOIN t_part p ON p.id = q.part_id \
             WHERE q.deleted_at IS NULL \
               AND ($1::text IS NULL OR q.status = $1) \
               AND (cardinality($2::text[]) = 0 OR q.status = ANY($2)) \
               AND ($3::bigint IS NULL OR q.part_id = $3) \
               AND (cardinality($4::bigint[]) = 0 OR q.part_id = ANY($4)) \
               AND ($5::bigint IS NULL OR q.outsource_company_id = $5) \
               AND ($6::text IS NULL OR p.drawing_no ILIKE $6) \
               AND ($7::text IS NULL OR p.name ILIKE $7) \
               AND ($8::boolean IS NULL OR p.is_urgent = $8) \
             ORDER BY \
               CASE WHEN $9::text = 'PRICE' AND $10::text = 'DESC' THEN q.price END DESC NULLS LAST, \
               CASE WHEN $9::text = 'PRICE' AND $10::text <> 'DESC' THEN q.price END ASC NULLS LAST, \
               CASE WHEN $9::text = 'REVIEWED_AT' AND $10::text = 'ASC' THEN q.reviewed_at END ASC NULLS LAST, \
               CASE WHEN $9::text = 'REVIEWED_AT' AND $10::text <> 'ASC' THEN q.reviewed_at END DESC NULLS LAST, \
               CASE WHEN $9::text = 'CREATED_AT' AND $10::text = 'ASC' THEN q.created_at END ASC, \
               q.created_at DESC, \
               q.id DESC \
             LIMIT $11 OFFSET $12",
        )
        .bind(status)
        .bind(statuses)
        .bind(part_id)
        .bind(part_ids_in)
        .bind(outsource_company_id)
        .bind(drawing_no_pat)
        .bind(name_pat)
        .bind(is_urgent)
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
        drawing_no_pat: Option<&str>,
        name_pat: Option<&str>,
        is_urgent: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_outsource_quote q \
             LEFT JOIN t_part p ON p.id = q.part_id \
             WHERE q.deleted_at IS NULL \
               AND ($1::text IS NULL OR q.status = $1) \
               AND (cardinality($2::text[]) = 0 OR q.status = ANY($2)) \
               AND ($3::bigint IS NULL OR q.part_id = $3) \
               AND (cardinality($4::bigint[]) = 0 OR q.part_id = ANY($4)) \
               AND ($5::bigint IS NULL OR q.outsource_company_id = $5) \
               AND ($6::text IS NULL OR p.drawing_no ILIKE $6) \
               AND ($7::text IS NULL OR p.name ILIKE $7) \
               AND ($8::boolean IS NULL OR p.is_urgent = $8)",
        )
        .bind(status)
        .bind(statuses)
        .bind(part_id)
        .bind(part_ids_in)
        .bind(outsource_company_id)
        .bind(drawing_no_pat)
        .bind(name_pat)
        .bind(is_urgent)
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

    /// 提交：DRAFT → SUBMITTED，写 submitted_at。
    ///
    /// ⚠️ `version` 形参**必须来自调用方**（见 `service/quote.rs::submit_quote`）。
    /// 若像本轮之前那样让 service 先 `quote_get_by_id` 读到当前 version 再喂进来，
    /// `WHERE id AND version = <刚读到的>` 在同一行上恒成立 ⇒ 守卫形同虚设。
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
    ///
    /// ⚠️ 本方法与 `list_for_company` 各自有一份**形状相同的谓词串**（形参收成
    /// `OutsourceSentPartFilter` 后仍各写一遍，因为 list 还要排序列）。加筛选维度时
    /// 必须两处同改 —— 漏改 count 的症状是 `items` 少了一截而 `total` 不变，
    /// 分页条数与总数对不上。
    pub async fn count_for_company<'e, E: PgExecutor<'e>>(
        executor: E,
        company_id: i64,
        filter: &OutsourceSentPartFilter<'_>,
    ) -> Result<i64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_outsource_shipment s \
             LEFT JOIN t_part p ON p.id = s.part_id \
             WHERE s.deleted_at IS NULL AND s.status IN ('OUTSOURCING', 'RECEIVED') \
               AND s.outsource_company_id = $1 \
               AND ($2::text IS NULL OR p.drawing_no ILIKE $2) \
               AND ($3::text IS NULL OR p.name ILIKE $3) \
               AND ($4::bigint IS NULL OR p.customer_id = $4) \
               AND ($5::bigint IS NULL OR s.process_id = $5) \
               AND ($6::boolean IS NULL OR s.is_billed = $6) \
               AND ($7::timestamp IS NULL OR s.sent_at >= $7) \
               AND ($8::timestamp IS NULL OR s.sent_at <= $8) \
               AND ($9::timestamp IS NULL OR s.received_at >= $9) \
               AND ($10::timestamp IS NULL OR s.received_at <= $10)",
        )
        .bind(company_id)
        .bind(filter.drawing_no)
        .bind(filter.name)
        .bind(filter.customer_id)
        .bind(filter.process_id)
        .bind(filter.is_billed)
        .bind(filter.sent_from)
        .bind(filter.sent_to)
        .bind(filter.received_from)
        .bind(filter.received_to)
        .fetch_one(executor)
        .await?;
        Ok(n)
    }

    /// 对账页 list（行 = shipment，JOIN 补齐展示字段）。
    ///
    /// 展示字段（part 图号/名称/加急、客户路径、工序名、批次号）**一次 JOIN 拿完**，
    /// service 层不再逐行回查（防 N+1）。
    ///
    /// `sort_by` / `sort_dir` 以**归一化后的白名单 token** 走 bind（`$11` / `$12`），
    /// 用 CASE 表达式选列 —— 用户输入永远不进 SQL 文本。
    ///
    /// 2026-10-09：投影去掉 `s.quote_id` / `s.part_id`（无消费方），WHERE 去掉
    /// `s.part_id = ANY($2)` 的 part_ids 预搜索，换成 `drawing_no` / `name` 两个
    /// 直连 ILIKE + `customer_id` / `process_id` / `is_billed` 三个精确谓词。
    /// `LEFT JOIN t_part p` 本就存在（取 `drawing_no` / `name` / `is_urgent`），
    /// 故 `p.customer_id` 的谓词是**零 JOIN 改动**。
    pub async fn list_for_company<'e, E: PgExecutor<'e>>(
        executor: E,
        company_id: i64,
        filter: &OutsourceSentPartFilter<'_>,
        sort_by: &str,
        sort_dir: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceSentPartRow>, sqlx::Error> {
        sqlx::query_as::<_, OutsourceSentPartRow>(
            "SELECT s.id, s.version, \
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
               AND ($2::text IS NULL OR p.drawing_no ILIKE $2) \
               AND ($3::text IS NULL OR p.name ILIKE $3) \
               AND ($4::bigint IS NULL OR p.customer_id = $4) \
               AND ($5::bigint IS NULL OR s.process_id = $5) \
               AND ($6::boolean IS NULL OR s.is_billed = $6) \
               AND ($7::timestamp IS NULL OR s.sent_at >= $7) \
               AND ($8::timestamp IS NULL OR s.sent_at <= $8) \
               AND ($9::timestamp IS NULL OR s.received_at >= $9) \
               AND ($10::timestamp IS NULL OR s.received_at <= $10) \
             ORDER BY \
               CASE WHEN $11::text = 'PRICE' AND $12::text = 'ASC' THEN s.unit_price END ASC NULLS LAST, \
               CASE WHEN $11::text = 'PRICE' AND $12::text <> 'ASC' THEN s.unit_price END DESC NULLS LAST, \
               CASE WHEN $11::text = 'RECEIVED_AT' AND $12::text = 'ASC' THEN s.received_at END ASC NULLS LAST, \
               CASE WHEN $11::text = 'RECEIVED_AT' AND $12::text <> 'ASC' THEN s.received_at END DESC NULLS LAST, \
               CASE WHEN $11::text = 'SENT_AT' AND $12::text = 'ASC' THEN s.sent_at END ASC, \
               CASE WHEN $11::text = 'SENT_AT' AND $12::text <> 'ASC' THEN s.sent_at END DESC, \
               s.id DESC \
             LIMIT $13 OFFSET $14",
        )
        .bind(company_id)
        .bind(filter.drawing_no)
        .bind(filter.name)
        .bind(filter.customer_id)
        .bind(filter.process_id)
        .bind(filter.is_billed)
        .bind(filter.sent_from)
        .bind(filter.sent_to)
        .bind(filter.received_from)
        .bind(filter.received_to)
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
// Sendable（候选侧谓词唯一真源；2026-10-09 起只服务外协看板）
// ===========================================================================

// ---------------------------------------------------------------------------
// 候选侧核心 SQL（**唯一真源**，2026-10-03 抽出）
//
// 背景：`GET /outsource-queue/snapshot`（按工序分组计数）与
// `GET /outsource-queue/processes/{id}`（按工序取全量）问的是**同一个集合**，只是外层
// 过滤与投影不同。判定谓词（批次状态三态 / OUTSOURCE 类别 / 审批闸门 /
// APPROVED 报价 LEFT JOIN）在每个查询里各写一份的话，任一改动漏改一处，前端就会看到
// 「看板 tab 徽标数与 tab 内实际行数对不上」。
//
// 故把「产出行」的部分抽成常量 + 参数化投影：
// - `SENDABLE_INNER_X_SQL`：JOIN 与 WHERE（**谓词只有这一个落点**）
// - `SENDABLE_DISTINCT_D_SQL`：`DISTINCT ON (batch_id, current_process_id)` 收敛
// - `SENDABLE_PROJECTION_*` / `SENDABLE_DEDUP_PROJECTION_*`：投影列表
//   （看板候选卡用全投影，看板按工序分组计数用精简投影）
// 调用方（`../board/repo.rs` 的 `SQL_SENDABLE_COUNT_BY_PROCESS` /
// `SQL_CANDIDATES_BY_PROCESS`）只换投影与外层过滤，谓词一行都不重复。
//
// 2026-10-09：`GET /outsource-sendable`（分页 list + count）硬切下线，它的两个外层
// 常量（`SENDABLE_OUTER_COLS` / `SENDABLE_DISPLAY_ORDER`）与 `list` / `count` 方法随之
// 删除 —— 看板候选卡有自己的外层列清单与展示序（`board/repo.rs`
// `SQL_CANDIDATES_BY_PROCESS`）。谓词与收敛层原样保留。
//
// `DISTINCT ON` 不要求分组键出现在投影里（与 `SELECT DISTINCT` 不同），
// 故分组计数可用精简投影（只要分组键 + 过滤列）而不违反任何约束。
// ---------------------------------------------------------------------------

/// 可发送外协查询的内层 `x`：一行 = 一个 `(batch, approved_quote)` 组合。
///
/// `{projection}` 由调用方填（全投影 / count 精简投影）；**JOIN 与 WHERE
/// 只有这一份**，四个查询共用。
///
/// ## 判定谓词 = 「批次当前停在某道外协工序上」
/// 工序来源是 `pb.current_process_id`（`JOIN t_process pr ON pr.id =
/// pb.current_process_id AND pr.category = 'OUTSOURCE'`）—— 那一列是**工序候选池
/// 归属的权威依据**（写入不变式见 `src/shared/batch/guards.rs`）。
///
/// 2026-10-03 移除的两层 JOIN：原实现要求「该 OUTSOURCE 工序同时出现在零件的
/// `process_chain_id` 链内」（`JOIN t_process_chain_step pcs`），而生产库里 1874 个
/// 零件只有 2 个绑了链、`t_process_chain_step` 里 OUTSOURCE 类的 step 有 0 条 ⇒
/// 交集恒空 ⇒ 端点恒返回空列表。同一类 bug 在 `prod::queue` 的候选池 SQL 上
/// 已于 2026-09-30 以同样方式修过（全仓已无 INNER JOIN `t_process_chain_step` 残留）。
/// 业务决策：**兼容没有工序链的旧零件**，`current_process_id` 指外协工序即可发。
///
/// ## 审批闸门（`t_process.requires_approval`）
/// 该列此前是**只写不读的死字段**（process CRUD 在维护、outsource 域从未读）。本查询
/// 是第一次真正使用它：
/// - `requires_approval = false` → 免审批直发，直接出行；
/// - `requires_approval = true` → 必须已有该 (part, process) 的**真实审批**报价，
///   否则**不出行**（回归网见 `tests/outsource/pool.rs` 的
///   `detail_requires_approval_without_quote_excluded` / `…_with_draft_quote_excluded`）。
///
/// 报价的 LEFT JOIN 条件里带 `AND pr.requires_approval`：它把「命中报价」严格定义成
/// 「send_mode = APPROVAL」。否则免审批工序上恰好存在一条历史 APPROVED 报价时，该行
/// 会被判成 APPROVAL（报价三件套有值）而 `company_options` 又被 CASE 短路成 `[]` ——
/// 两种模式的字段契约同时被破坏。DIRECT 行的 `quote_id` / `price` / 公司三件套因此
/// 恒为 `null`，与 VO 声明一致。
///
/// ## 2026-10-03 review 第 1 轮：`AND is_direct = false`（两处谓词都要带）
/// 「APPROVED 报价」不等于「被人审批过的报价」：DIRECT 直发路径会自动建
/// `status='APPROVED' AND is_direct=true AND price=0` 的占位报价（见
/// `service/move.rs::resolve_direct_quote_id`）。只判
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
/// ## 2026-10-09 新增：申请人走 `LEFT JOIN LATERAL`
/// 外协看板候选卡要显示申请人**已录制的名字**（`t_applicant.name`），而
/// `t_part.applicant_name` 是字符串非 FK、`t_applicant` 的唯一索引是
/// `(name, customer_id)` —— **name 单独不唯一**，同名申请人跨客户并存时直 JOIN 会
/// 把一行扇成多行。`LEFT JOIN LATERAL (… ORDER BY ap.id ASC LIMIT 1)` 把扇行压回一行：
/// 投影只有 `ap.name` 一个列且由 WHERE 保证恒等 ⇒ 取哪一行取值都一样，`ORDER BY` 的
/// 作用只是给这条 LATERAL 一个确定的行，零语义内容。
/// 本层本来就有 `DISTINCT ON (batch_id, current_process_id)` 兜底（不会把重复行带
/// 出结果集），但 LATERAL 让收敛层不必依赖「收敛键恰好覆盖扇行来源」这条隐式事实。
/// 同一取舍的在途侧版本见 `board/repo.rs::SQL_HELD_BY_PROCESS`（那里没有 DISTINCT
/// ON 兜底，LATERAL 是唯一防线）。
///
/// ## 为什么 `t_shelf` 降级成 LEFT JOIN
/// 外层投影仍要 `shelf_code`（前端看板卡片要显示批次在哪排），但 `PENDING` 批次的
/// `current_holder_id` 恒为 `NULL`（还没上架）—— 若保持 INNER JOIN，未上架的
/// PENDING 批次会整批消失。`shelf_code` 相应改为可空（VO 本来就是 `Option`）。
///
/// ## `pb.status = 'PENDING'` 这一析取项在写侧不可达（不是 bug，别按可达路径核对）
/// 写入不变式：PENDING ⇔ 出池（`clear_process_id` 把 `current_process_id` 置 NULL，
/// 见 `src/shared/batch/guards.rs::mark_batch_with_status_and_meta` 的 doc），
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
             LEFT JOIN LATERAL ( \
               SELECT ap.name \
               FROM t_applicant ap \
               WHERE ap.name = p.applicant_name AND ap.deleted_at IS NULL \
               ORDER BY ap.id ASC \
               LIMIT 1 \
             ) ap1 ON TRUE \
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

/// 全投影（看板候选卡 `board/repo.rs::CandidateRow` 的解码目标）。
pub(crate) const SENDABLE_PROJECTION_FULL: &str = "pb.version AS batch_version, pb.id AS batch_id, \
     pb.batch_no, pb.quantity AS batch_quantity, pb.status AS source_status, \
     pb.current_holder_id AS shelf_id, \
     p.id AS part_id, p.serial_no AS part_serial_no, \
     p.drawing_no AS part_drawing_no, p.name AS part_name, p.note, \
     to_char(p.planned_delivery_date, 'YYYY-MM-DD') AS planned_delivery_date, \
     to_char(p.system_delivery_date, 'YYYY-MM-DD') AS system_delivery_date, \
     p.is_urgent, p.customer_id, \
     c.name AS customer_name, cp.name AS parent_customer_name, \
     ap1.name AS applicant_name, \
     EXISTS (SELECT 1 FROM t_part_file pf \
             WHERE pf.part_id = pb.part_id \
               AND pf.kind = 'G_CODE' \
               AND pf.deleted_at IS NULL) AS has_cnc_program, \
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
pub(crate) const SENDABLE_DEDUP_PROJECTION_FULL: &str = "x.batch_version, x.batch_id, x.batch_no, \
     x.batch_quantity, x.source_status, x.shelf_id, \
     x.part_id, x.part_serial_no, x.part_drawing_no, x.part_name, x.note, \
     x.planned_delivery_date, x.system_delivery_date, x.is_urgent, \
     x.customer_name, x.parent_customer_name, x.customer_id, \
     x.applicant_name, x.has_cnc_program, \
     x.shelf_code, x.current_process_id, x.current_process_name, x.requires_approval, \
     x.quote_id, x.price, x.outsource_company_id, x.outsource_company_name, \
     x.company_options";

/// 分组计数侧精简投影：只要分组键 `(batch_id, current_process_id)` + 收敛排序键
/// `quote_id`。
///
/// 语义等价前提：分组键内 part 列恒定（同一批次恒同一零件，故这些列组内不变）。
pub(crate) const SENDABLE_PROJECTION_COUNT: &str = "pb.id AS batch_id, \
     p.drawing_no AS part_drawing_no, p.name AS part_name, p.customer_id, \
     pr.id AS current_process_id, q.id AS quote_id";

pub(crate) const SENDABLE_DEDUP_PROJECTION_COUNT: &str = "x.batch_id, x.current_process_id, \
     x.part_drawing_no, x.part_name, x.customer_id";

/// 拼出 `x → d` 两层子查询（供外层复用）。
///
/// ⚠️ **注入面为 0**：本函数只把调用方给的**编译期常量**（`SENDABLE_PROJECTION_*`
/// / `SENDABLE_DEDUP_PROJECTION_*`）填进 `{projection}` / `{inner}` 占位符，
/// 全程不接触任何用户输入；用户输入（`process_id`）一律走 bind（`$1`）。故两个消费方
/// 用 `AssertSqlSafe(sql)` 包裹动态 SQL 文本是安全的（口径同
/// `com::union_list::repo::sql`）。
pub(crate) fn sendable_dedup_sql(inner_projection: &str, dedup_projection: &str) -> String {
    let inner = SENDABLE_INNER_X_SQL.replace("{projection}", inner_projection);
    SENDABLE_DISTINCT_D_SQL
        .replace("{projection}", dedup_projection)
        .replace("{inner}", &inner)
}

// ===========================================================================
// 「下一道工序」推导（LATERAL 片段，2026-10-09 抽成共用常量）
// ===========================================================================

/// 「当前工序的下一道工序」推导片段，供**两个**调用方 `LEFT JOIN LATERAL ... nx ON
/// TRUE`（别名固定为 `nx`，输出两列 `next_process_id` / `next_process_name`）。
///
/// ## 两个调用方（必须共用同一份口径）
/// 1. `../board/repo.rs::SQL_HELD_BY_PROCESS` —— 在途卡片的
///    `receive_next_process_id` / `receive_next_process_name` 与
///    `chain_resolvable`（= `receive_next_process_id != 0`）；
/// 2. `../service/move.rs` —— `POST /outsource-queue/move` 省略
///    `to.next_process_id` 时的服务端推导。
///
/// 它们问的是同一个事实（「这批货从外协收回后该进哪道工序」），各写一份时前端会
/// 遇到「看板上显示可免填、写端点却返 20706」或反之 —— 两处都**不报错**，只是让
/// 运营在对话框里反复试。
///
/// ## 两步定位
/// 1. **锚链** = `COALESCE(p.process_chain_id, cur.chain_id)`（`cur` =
///    `pb.current_process_step_id` 指向的 step，只用于回退取链 id）；
/// 2. **当前 step 在锚链内的位置**：`cur2.process_id = pb.current_process_id`；
///    再取锚链内 `sort_order = cur2.sort_order + 1` 的未软删 step（唯一索引
///    `uq_chain_step_chain_order (chain_id, sort_order) WHERE deleted_at IS NULL`
///    ⇒ 唯一无歧义）。中间 JOIN `t_part_process_chain` 是为了让「锚链已软删」也落到
///    「无下一 step」分支。
///
/// ⚠️ **第 2 步必须按 `current_process_id` 在锚链内重新定位，不能拿
/// `pb.current_process_step_id` 的 `sort_order` 直接当位置**：step 指针与「当前工序在
/// 链内的位置」是两个独立事实，后者漂移时按位置推进会把**外协工序自己**当成下一道
/// 工序返回，而 `chain_resolvable` 仍在说「可免填」⇒ 写侧照单全收，静默错值比拒收
/// 更容易发现不了。
///
/// **锚链与写侧 step 解析同源**：写侧进生产流走
/// `optional_process_chain(part_id)`（读 `t_part.process_chain_id`）+
/// `optional_step_id(chain_id, process_id)`。锚 `p.process_chain_id` ⇒ 本片段返回的
/// process_id 必然是**锚链内活跃 step 的工序**，写侧能在同一条链上解析到（有链时）。
///
/// `COALESCE(p.process_chain_id, cur.chain_id)` 的回落分支服务于**无链批次**
/// （`p.process_chain_id IS NULL`）：这类零件可发外协也就可能在途，其
/// `current_process_step_id` 按写入不变式恒为 NULL ⇒ `cur` 子查询无行 ⇒ 锚链解析
/// 失败 ⇒ 推不出下一道工序（看板 `chain_resolvable = false`；写端点收 `to` 侧
/// `next_process_id` 省略时返 `20706`）。保留 `COALESCE` 是因为读侧不假设写侧何时
/// 写 step，它守的是「无链」这一常态。
///
/// ⚠️ **两个派生列都必须显式 `AS receive_next_process_*`（看板侧）**：LATERAL 子查询
/// 的输出列名只跟子查询内部的名字走（`nx.next_process_name` 的列名是
/// `next_process_name`，不带 `nx.` 前缀），不写别名时 runtime `query_as` 的
/// `FromRow` 会报 `ColumnNotFound`。末尾 `LIMIT 1` 保证 LATERAL 恒至多一行：锚链内
/// 同一 `process_id` 重复属数据异常，写侧 `resolve_step_id_by_process` 自己登记的
/// 立场逐字是「链内同一 process_id 重复（数据异常）的歧义不在本函数守」，本片段
/// 沿用同一口径收口；不设 `LIMIT` 会把一行批次扇成多行（看板侧还会破坏
/// `held_count == held_batches.len()`）。
///
/// **外层必须把这两个别名原样 SELECT 出去**（看板侧 `AS receive_next_process_*`、
/// 推导侧见 `../service/move.rs` 的 `SQL_DERIVE_NEXT_PROCESS`），否则列名对不上
/// `FromRow`。
pub(crate) const NEXT_PROCESS_LATERAL_SQL: &str = "LEFT JOIN LATERAL ( \
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
   ) nx ON TRUE";
