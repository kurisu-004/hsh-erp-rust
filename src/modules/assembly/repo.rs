//! assembly 域数据访问
//!
//! 对应 Python myERP/repository/assembly_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P1（送货分组）只暴露 delivery_note + delivery_group 后续会用到的只读点：
//! - `get_by_id` / `list_by_ids`
//! - `get_by_serial` —— 扫码入单解析（Phase P3）需要 exact match

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use super::model::TAssembly;

pub struct AssemblyRepo;

impl AssemblyRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TAssembly>, sqlx::Error> {
        sqlx::query_as!(
            TAssembly,
            r#"
            SELECT id, drawing_no, name, applicant_name, customer_id,
                   request_date, planned_delivery_date, actual_delivery_date,
                   is_urgent, status, version, created_at, created_by,
                   updated_at, updated_by, deleted_at, serial_no, quantity,
                   unit_price, total_price, order_no, system_delivery_date, note
            FROM t_assembly
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        ids: &[i64],
        include_deleted: bool,
    ) -> Result<Vec<TAssembly>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TAssembly,
            r#"
            SELECT id, drawing_no, name, applicant_name, customer_id,
                   request_date, planned_delivery_date, actual_delivery_date,
                   is_urgent, status, version, created_at, created_by,
                   updated_at, updated_by, deleted_at, serial_no, quantity,
                   unit_price, total_price, order_no, system_delivery_date, note
            FROM t_assembly
            WHERE id = ANY($1)
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            ids,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// 按 `serial_no` exact match 查（扫码定位用）。
    /// `t_assembly` 上有 partial unique（`uk_t_assembly_serial_no`），活跃行只可能一条。
    pub async fn get_by_serial<'e, E: PgExecutor<'e>>(
        executor: E,
        serial_no: &str,
        include_deleted: bool,
    ) -> Result<Option<TAssembly>, sqlx::Error> {
        sqlx::query_as!(
            TAssembly,
            r#"
            SELECT id, drawing_no, name, applicant_name, customer_id,
                   request_date, planned_delivery_date, actual_delivery_date,
                   is_urgent, status, version, created_at, created_by,
                   updated_at, updated_by, deleted_at, serial_no, quantity,
                   unit_price, total_price, order_no, system_delivery_date, note
            FROM t_assembly
            WHERE serial_no = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            serial_no,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }
}

// ---------- INSERT ----------

pub struct NewAssembly<'a> {
    pub id: i64,
    pub drawing_no: &'a str,
    pub name: &'a str,
    pub applicant_name: Option<&'a str>,
    pub customer_id: i64,
    pub request_date: Option<NaiveDate>,
    pub planned_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    pub status: &'a str,
    pub version: i32,
    pub serial_no: Option<&'a str>,
    pub quantity: i32,
    pub unit_price: Option<Decimal>,
    pub total_price: Option<Decimal>,
    pub order_no: Option<&'a str>,
    pub system_delivery_date: Option<NaiveDate>,
    pub note: Option<&'a str>,
    pub created_by: i64,
}

impl AssemblyRepo {
    pub async fn insert<'e, E: PgExecutor<'e>>(
        executor: E,
        new: NewAssembly<'_>,
    ) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            r#"
            INSERT INTO t_assembly (
                id, drawing_no, name, applicant_name, customer_id,
                request_date, planned_delivery_date, is_urgent, status, version,
                serial_no, quantity, unit_price, total_price,
                order_no, system_delivery_date, note, created_by
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8, $9, $10,
                $11, $12, $13, $14,
                $15, $16, $17, $18
            )
            RETURNING id
            "#,
        )
        .bind(new.id)
        .bind(new.drawing_no)
        .bind(new.name)
        .bind(new.applicant_name)
        .bind(new.customer_id)
        .bind(new.request_date)
        .bind(new.planned_delivery_date)
        .bind(new.is_urgent)
        .bind(new.status)
        .bind(new.version)
        .bind(new.serial_no)
        .bind(new.quantity)
        .bind(new.unit_price)
        .bind(new.total_price)
        .bind(new.order_no)
        .bind(new.system_delivery_date)
        .bind(new.note)
        .bind(new.created_by)
        .fetch_one(executor)
        .await?;
        Ok(row.0)
    }
}

// ---------- UPDATE partial ----------

pub struct AssemblyUpdate<'a> {
    pub drawing_no: Option<&'a str>,
    pub name: Option<&'a str>,
    pub applicant_name: Option<Option<&'a str>>,
    pub customer_id: Option<i64>,
    pub request_date: Option<Option<NaiveDate>>,
    pub planned_delivery_date: Option<Option<NaiveDate>>,
    pub actual_delivery_date: Option<Option<NaiveDate>>,
    pub is_urgent: Option<bool>,
    pub quantity: Option<i32>,
    pub unit_price: Option<Option<Decimal>>,
    pub total_price: Option<Option<Decimal>>,
    pub order_no: Option<Option<&'a str>>,
    pub system_delivery_date: Option<Option<NaiveDate>>,
    pub note: Option<Option<&'a str>>,
    pub updated_by: i64,
}

impl AssemblyRepo {
    pub async fn update_partial<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        expected_version: i32,
        upd: AssemblyUpdate<'_>,
    ) -> Result<u64, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "UPDATE t_assembly SET version = version + 1, updated_at = NOW(), updated_by = ",
        );
        qb.push_bind(upd.updated_by);
        let mut sep: &'static str = " , ";
        let push_opt_str = |qb: &mut QueryBuilder<Postgres>,
                                col: &str,
                                v: Option<&str>,
                                sep: &mut &'static str| {
            if let Some(val) = v {
                qb.push(*sep).push(col).push(" = ").push_bind(val.to_string());
                *sep = " , ";
            }
        };
        let push_opt_opt_str = |qb: &mut QueryBuilder<Postgres>,
                                    col: &str,
                                    v: Option<Option<&str>>,
                                    sep: &mut &'static str| {
            if let Some(opt) = v {
                qb.push(*sep).push(col).push(" = ");
                match opt {
                    Some(val) => {
                        qb.push_bind(val.to_string());
                    }
                    None => {
                        qb.push("NULL");
                    }
                }
                *sep = " , ";
            }
        };
        let push_opt_opt_date = |qb: &mut QueryBuilder<Postgres>,
                                     col: &str,
                                     v: Option<Option<NaiveDate>>,
                                     sep: &mut &'static str| {
            if let Some(opt) = v {
                qb.push(*sep).push(col).push(" = ");
                match opt {
                    Some(val) => {
                        qb.push_bind(val);
                    }
                    None => {
                        qb.push("NULL");
                    }
                }
                *sep = " , ";
            }
        };
        let push_opt_opt_dec = |qb: &mut QueryBuilder<Postgres>,
                                    col: &str,
                                    v: Option<Option<Decimal>>,
                                    sep: &mut &'static str| {
            if let Some(opt) = v {
                qb.push(*sep).push(col).push(" = ");
                match opt {
                    Some(val) => {
                        qb.push_bind(val);
                    }
                    None => {
                        qb.push("NULL");
                    }
                }
                *sep = " , ";
            }
        };
        push_opt_str(&mut qb, "drawing_no", upd.drawing_no, &mut sep);
        push_opt_str(&mut qb, "name", upd.name, &mut sep);
        push_opt_opt_str(&mut qb, "applicant_name", upd.applicant_name, &mut sep);
        if let Some(cid) = upd.customer_id {
            qb.push(sep).push("customer_id = ").push_bind(cid);
            sep = " , ";
        }
        push_opt_opt_date(&mut qb, "request_date", upd.request_date, &mut sep);
        push_opt_opt_date(
            &mut qb,
            "planned_delivery_date",
            upd.planned_delivery_date,
            &mut sep,
        );
        push_opt_opt_date(
            &mut qb,
            "actual_delivery_date",
            upd.actual_delivery_date,
            &mut sep,
        );
        if let Some(u) = upd.is_urgent {
            qb.push(sep).push("is_urgent = ").push_bind(u);
            sep = " , ";
        }
        if let Some(q) = upd.quantity {
            qb.push(sep).push("quantity = ").push_bind(q);
            sep = " , ";
        }
        push_opt_opt_dec(&mut qb, "unit_price", upd.unit_price, &mut sep);
        push_opt_opt_dec(&mut qb, "total_price", upd.total_price, &mut sep);
        push_opt_opt_str(&mut qb, "order_no", upd.order_no, &mut sep);
        push_opt_opt_date(
            &mut qb,
            "system_delivery_date",
            upd.system_delivery_date,
            &mut sep,
        );
        push_opt_opt_str(&mut qb, "note", upd.note, &mut sep);

        qb.push(" WHERE id = ").push_bind(id)
            .push(" AND version = ").push_bind(expected_version)
            .push(" AND deleted_at IS NULL");
        let res = qb.build().execute(executor).await?;
        Ok(res.rows_affected())
    }
}

// ---------- soft_delete ----------

impl AssemblyRepo {
    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r#"
            UPDATE t_assembly
            SET version = version + 1, deleted_at = NOW(), updated_at = NOW(), updated_by = $3
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(id)
        .bind(expected_version)
        .bind(current_user_id)
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }
}

// ---------- cancel ----------

impl AssemblyRepo {
    pub async fn cancel<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r#"
            UPDATE t_assembly
            SET status = 'CANCELLED', version = version + 1, updated_at = NOW(), updated_by = $2
            WHERE id = $1 AND status NOT IN ('COMPLETED','CANCELLED') AND deleted_at IS NULL
            "#,
        )
        .bind(id)
        .bind(current_user_id)
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }
}

// ---------- list_with_filters + count_with_filters ----------

pub struct AssemblyListFilters<'a> {
    pub customer_ids: &'a [i64],
    pub status: Option<&'a str>,
    pub statuses: &'a [String],
    pub is_urgent: Option<bool>,
    pub keyword: Option<&'a str>,
    pub sort_by: Option<&'a str>,
    pub sort_dir: Option<&'a str>,
    pub limit: i64,
    pub offset: i64,
    pub include_deleted: bool,
}

fn assembly_sort_col(sort_by: Option<&str>) -> &'static str {
    match sort_by {
        Some("CREATED_AT") => "created_at",
        Some("UPDATED_AT") => "updated_at",
        Some("DRAWING_NO") => "drawing_no",
        Some("NAME") => "name",
        _ => "id",
    }
}

impl AssemblyRepo {
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        f: &AssemblyListFilters<'_>,
    ) -> Result<Vec<TAssembly>, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT id, drawing_no, name, applicant_name, customer_id, request_date, planned_delivery_date, actual_delivery_date, is_urgent, status, version, created_at, created_by, updated_at, updated_by, deleted_at, serial_no, quantity, unit_price, total_price, order_no, system_delivery_date, note FROM t_assembly WHERE 1=1",
        );
        if !f.include_deleted {
            qb.push(" AND deleted_at IS NULL");
        }
        if !f.customer_ids.is_empty() {
            qb.push(" AND customer_id IN (");
            let mut sep = qb.separated(", ");
            for cid in f.customer_ids {
                sep.push_bind(*cid);
            }
            qb.push(")");
        }
        if let Some(s) = f.status {
            qb.push(" AND status = ").push_bind(s.to_string());
        }
        if !f.statuses.is_empty() {
            qb.push(" AND status IN (");
            let mut sep = qb.separated(", ");
            for s in f.statuses {
                sep.push_bind(s.clone());
            }
            qb.push(")");
        }
        if let Some(u) = f.is_urgent {
            qb.push(" AND is_urgent = ").push_bind(u);
        }
        if let Some(k) = f.keyword {
            let pat = format!("%{}%", k.trim());
            qb.push(" AND (drawing_no ILIKE ").push_bind(pat.clone())
                .push(" OR name ILIKE ").push_bind(pat)
                .push(" OR serial_no ILIKE ").push_bind(format!("%{}%", k.trim()))
                .push(")");
        }
        let col = assembly_sort_col(f.sort_by);
        let dir = if f.sort_dir.map(|s| s.eq_ignore_ascii_case("ASC")).unwrap_or(false) { "ASC" } else { "DESC" };
        qb.push(" ORDER BY ").push(col).push(" ").push(dir);
        qb.push(" LIMIT ").push_bind(f.limit);
        qb.push(" OFFSET ").push_bind(f.offset);
        qb.build_query_as::<TAssembly>().fetch_all(executor).await
    }

    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        f: &AssemblyListFilters<'_>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT COUNT(*) FROM t_assembly WHERE 1=1");
        if !f.include_deleted {
            qb.push(" AND deleted_at IS NULL");
        }
        if !f.customer_ids.is_empty() {
            qb.push(" AND customer_id IN (");
            let mut sep = qb.separated(", ");
            for cid in f.customer_ids {
                sep.push_bind(*cid);
            }
            qb.push(")");
        }
        if let Some(s) = f.status {
            qb.push(" AND status = ").push_bind(s.to_string());
        }
        if !f.statuses.is_empty() {
            qb.push(" AND status IN (");
            let mut sep = qb.separated(", ");
            for s in f.statuses {
                sep.push_bind(s.clone());
            }
            qb.push(")");
        }
        if let Some(u) = f.is_urgent {
            qb.push(" AND is_urgent = ").push_bind(u);
        }
        if let Some(k) = f.keyword {
            let pat = format!("%{}%", k.trim());
            qb.push(" AND (drawing_no ILIKE ").push_bind(pat.clone())
                .push(" OR name ILIKE ").push_bind(pat)
                .push(" OR serial_no ILIKE ").push_bind(format!("%{}%", k.trim()))
                .push(")");
        }
        let row: (i64,) = qb.build_query_as().fetch_one(executor).await?;
        Ok(row.0)
    }
}