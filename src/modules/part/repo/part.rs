//! `t_part` 主表查询 + 状态机 UPDATE
//!
//! 既有 5 个只读查询（`get_by_id` / `list_by_ids` / `get_by_serial` /
//! `list_children` / `get_part_inspected`）保持原签名搬迁。
//!
//! Phase PR-CRUD 新增 6 个方法服务于 CRUD + lifecycle：
//! - `get_part_detail` —— 完整行（不含软删）；service 详情接口用
//! - `create_part` —— INSERT，初始 `status='PENDING'`
//! - `update_part` —— 字段可选 UPDATE（QueryBuilder 拼装）
//! - `soft_delete_part` —— `deleted_at = now()`；守卫 `status NOT IN
//!   ('DELIVERED','COMPLETED')` 且无 `delivery_note_id`
//! - `list_with_filters` / `count_with_filters` —— 列表筛选 + 分页

use sqlx::PgExecutor;

use crate::modules::part::model::{TPart, TPartInspected};

use super::PartRepo;

/// `create_part` 输入：service 层用 builder 模式注入。
///
/// `id` 由 caller 预生成雪花；`status` 初始为 `'PENDING'`；`location` /
/// `current_holder_id` / `next_process_id` / `serial_no` / `delivery_note_id` /
/// `actual_delivery_date` / `deleted_at` / `version` / `has_been_repaired` 走
/// DB 默认或 `NULL`。
pub struct NewPartCreate<'a> {
    pub id: i64,
    pub name: &'a str,
    pub drawing_no: &'a str,
    pub applicant_name: &'a str,
    pub quantity: i32,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    pub is_urgent: bool,
    pub customer_id: i64,
    pub assembly_id: Option<i64>,
    pub order_no: Option<&'a str>,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub note: Option<&'a str>,
    pub created_by: i64,
}

/// `update_part` 输入：所有字段 `Option`，未设置的字段不动。
///
/// `version += 1` 与 `updated_at = now()` 强制写入；`updated_by` 必填。
pub struct PartUpdate<'a> {
    pub name: Option<&'a str>,
    pub drawing_no: Option<&'a str>,
    pub applicant_name: Option<&'a str>,
    pub quantity: Option<i32>,
    pub order_no: Option<&'a str>,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub actual_delivery_date: Option<chrono::NaiveDate>,
    pub note: Option<&'a str>,
    pub is_urgent: Option<bool>,
    pub updated_by: i64,
}

/// `list_with_filters` / `count_with_filters` 输入。
///
/// 排序字段用字符串映射到白名单列名（防 SQL 注入），方向仅接受 `ASC` / `DESC`。
/// `status` 与 `statuses` 互不冲突：service 层按业务场景二选一传入。
#[derive(Debug, Default, Clone)]
pub struct PartListFilters<'a> {
    pub customer_ids: &'a [i64],
    pub status: Option<&'a str>,
    pub statuses: &'a [String],
    pub is_urgent: Option<bool>,
    pub keyword: Option<&'a str>,
    pub sort_by: &'a str,
    pub sort_dir: &'a str,
    pub limit: i64,
    pub offset: i64,
    pub include_deleted: bool,
}

impl PartRepo {
    // ===== 既有 5 个查询方法（搬迁自原 repo.rs，签名不变） =====

    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, applicant_name, quantity,
                   request_date, planned_delivery_date, actual_delivery_date,
                   customer_id, assembly_id, status, location,
                   is_urgent, current_holder_id, placed_at, next_process_id,
                   order_no, system_delivery_date, note, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, delivery_note_id
            FROM t_part
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
    ) -> Result<Vec<TPart>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, applicant_name, quantity,
                   request_date, planned_delivery_date, actual_delivery_date,
                   customer_id, assembly_id, status, location,
                   is_urgent, current_holder_id, placed_at, next_process_id,
                   order_no, system_delivery_date, note, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, delivery_note_id
            FROM t_part
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
    /// `serial_no` 在 DB 层有 partial unique（`uk_t_part_serial_no`），
    /// 活跃行只可能一条；include_deleted=false 时过滤掉软删件（扫码不应该命中软删）。
    pub async fn get_by_serial<'e, E: PgExecutor<'e>>(
        executor: E,
        serial_no: &str,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, applicant_name, quantity,
                   request_date, planned_delivery_date, actual_delivery_date,
                   customer_id, assembly_id, status, location,
                   is_urgent, current_holder_id, placed_at, next_process_id,
                   order_no, system_delivery_date, note, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, delivery_note_id
            FROM t_part
            WHERE serial_no = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            serial_no,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 装配件的子件列表，按 id 升序（Phase P3 扫码整套入单需要）。
    pub async fn list_children<'e, E: PgExecutor<'e>>(
        executor: E,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, applicant_name, quantity,
                   request_date, planned_delivery_date, actual_delivery_date,
                   customer_id, assembly_id, status, location,
                   is_urgent, current_holder_id, placed_at, next_process_id,
                   order_no, system_delivery_date, note, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, delivery_note_id
            FROM t_part
            WHERE assembly_id = $1
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            assembly_id,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// pass_inspection 流专用最小投影（Phase F）。
    ///
    /// `include_deleted = false`（pass_inspection 不应对软删件操作）。
    pub async fn get_part_inspected<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<Option<TPartInspected>, sqlx::Error> {
        sqlx::query_as!(
            TPartInspected,
            r#"
            SELECT id, serial_no, name, drawing_no, status, version, quantity,
                   order_no, actual_delivery_date, current_holder_id,
                   updated_at, updated_by
            FROM t_part
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            part_id,
        )
        .fetch_optional(executor)
        .await
    }

    // ===== Phase PR-CRUD 新增 =====

    /// 详情接口：完整 28 列行（含软删检测）。
    ///
    /// service 层在 `get_by_id(..., include_deleted=false)` 失败时可用本方法
    /// 做兜底（含软删场景）以区分「不存在」与「已软删」。
    pub async fn get_part_detail<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<Option<TPart>, sqlx::Error> {
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, applicant_name, quantity,
                   request_date, planned_delivery_date, actual_delivery_date,
                   customer_id, assembly_id, status, location, is_urgent,
                   current_holder_id, placed_at, next_process_id,
                   order_no, system_delivery_date, note, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, delivery_note_id
            FROM t_part
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            part_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// INSERT `t_part`：初始 `status='PENDING'`、`version=0`（DB 默认）。
    ///
    /// 返回写入行的雪花 `id`（与 `new.id` 一致；此处显式 `RETURNING id`
    /// 以兼容未来可能的 trigger 重写 id 的场景）。
    pub async fn create_part<'e, E: PgExecutor<'e>>(
        executor: E,
        new: NewPartCreate<'_>,
    ) -> Result<i64, sqlx::Error> {
        let id: i64 = sqlx::query_scalar!(
            r#"
            INSERT INTO t_part (
                id, name, drawing_no, applicant_name, quantity,
                request_date, planned_delivery_date,
                status, is_urgent, customer_id, assembly_id,
                order_no, system_delivery_date, note,
                created_at, created_by, updated_at, updated_by
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7,
                'PENDING', $8, $9, $10,
                $11, $12, $13,
                now(), $14, now(), $14
            )
            RETURNING id AS "id!"
            "#,
            new.id, new.name, new.drawing_no, new.applicant_name, new.quantity,
            new.request_date, new.planned_delivery_date, new.is_urgent,
            new.customer_id, new.assembly_id, new.order_no, new.system_delivery_date,
            new.note, new.created_by,
        )
        .fetch_one(executor)
        .await?;
        Ok(id)
    }

    /// 字段可选 UPDATE（QueryBuilder 拼装）；OCC + 软删守卫。
    ///
    /// 返回受影响行数：`1` = 成功，`0` = OCC 冲突 / 已软删 / `part_id` 不存在。
    /// `version += 1` 与 `updated_at = now()` 强制写入（与 `mark_*` 方法族对齐）。
    pub async fn update_part<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        upd: PartUpdate<'_>,
    ) -> Result<u64, sqlx::Error> {
        let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
            "UPDATE t_part SET version = version + 1, updated_at = now(), updated_by = ",
        );
        qb.push_bind(upd.updated_by);
        if let Some(v) = upd.name { qb.push(", name = ").push_bind(v.to_string()); }
        if let Some(v) = upd.drawing_no { qb.push(", drawing_no = ").push_bind(v.to_string()); }
        if let Some(v) = upd.applicant_name { qb.push(", applicant_name = ").push_bind(v.to_string()); }
        if let Some(v) = upd.quantity { qb.push(", quantity = ").push_bind(v); }
        if let Some(v) = upd.order_no { qb.push(", order_no = ").push_bind(v.to_string()); }
        if let Some(v) = upd.system_delivery_date { qb.push(", system_delivery_date = ").push_bind(v); }
        if let Some(v) = upd.planned_delivery_date { qb.push(", planned_delivery_date = ").push_bind(v); }
        if let Some(v) = upd.actual_delivery_date { qb.push(", actual_delivery_date = ").push_bind(v); }
        if let Some(v) = upd.note { qb.push(", note = ").push_bind(v.to_string()); }
        if let Some(v) = upd.is_urgent { qb.push(", is_urgent = ").push_bind(v); }
        qb.push(" WHERE id = ").push_bind(part_id);
        qb.push(" AND version = ").push_bind(expected_version);
        qb.push(" AND deleted_at IS NULL");
        let r = qb.build().execute(executor).await?;
        Ok(r.rows_affected())
    }

    /// 软删：`deleted_at = now()` + `version += 1`。
    ///
    /// 守卫：`status NOT IN ('DELIVERED','COMPLETED')` 且 `delivery_note_id IS NULL`
    /// —— 已签收 / 已完结 / 已绑定送货单的工单不允许软删，由 service 层根据
    /// 返回行数判断并映射错误码。
    pub async fn soft_delete_part<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"
            UPDATE t_part
            SET deleted_at = now(),
                version    = version + 1,
                updated_at = now(),
                updated_by = $3
            WHERE id = $1 AND version = $2
              AND status NOT IN ('DELIVERED', 'COMPLETED')
              AND delivery_note_id IS NULL
              AND deleted_at IS NULL
            "#,
            part_id, expected_version, current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 列表筛选 + 分页 + 排序。
    ///
    /// 排序字段白名单（防 SQL 注入）：CREATED_AT / UPDATED_AT /
    /// PLANNED_DELIVERY_DATE / REQUEST_DATE / SERIAL_NO / DRAWING_NO / NAME，
    /// 其它值退化为 `id`。方向仅接受 `ASC`，其它视为 `DESC`。
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        f: &PartListFilters<'_>,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        let order_col = match f.sort_by {
            "CREATED_AT" => "created_at",
            "UPDATED_AT" => "updated_at",
            "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
            "REQUEST_DATE" => "request_date",
            "SERIAL_NO" => "serial_no",
            "DRAWING_NO" => "drawing_no",
            "NAME" => "name",
            _ => "id",
        };
        let order_dir = if f.sort_dir.eq_ignore_ascii_case("ASC") { "ASC" } else { "DESC" };

        let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
            "SELECT id, serial_no, name, drawing_no, applicant_name, quantity, \
                    request_date, planned_delivery_date, actual_delivery_date, \
                    customer_id, assembly_id, status, location, is_urgent, \
                    current_holder_id, placed_at, next_process_id, \
                    order_no, system_delivery_date, note, has_been_repaired, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, delivery_note_id \
             FROM t_part WHERE 1=1",
        );
        if !f.include_deleted { qb.push(" AND deleted_at IS NULL"); }
        if !f.customer_ids.is_empty() {
            qb.push(" AND customer_id = ANY(").push_bind(f.customer_ids.to_vec()).push(")");
        }
        if let Some(s) = f.status { qb.push(" AND status = ").push_bind(s.to_string()); }
        if !f.statuses.is_empty() {
            let arr = f.statuses.to_vec();
            if arr.len() == 1 {
                qb.push(" AND status = ").push_bind(arr[0].clone());
            } else {
                qb.push(" AND status = ANY(").push_bind(arr).push(")");
            }
        }
        if let Some(u) = f.is_urgent { qb.push(" AND is_urgent = ").push_bind(u); }
        if let Some(k) = f.keyword {
            let pat = format!("%{}%", k.trim());
            qb.push(" AND (name ILIKE ").push_bind(pat.clone())
              .push(" OR drawing_no ILIKE ").push_bind(pat.clone())
              .push(" OR serial_no ILIKE ").push_bind(pat)
              .push(")");
        }
        qb.push(format!(" ORDER BY {order_col} {order_dir} NULLS LAST, id DESC"));
        qb.push(" LIMIT ").push_bind(f.limit);
        qb.push(" OFFSET ").push_bind(f.offset);
        qb.build_query_as::<TPart>().fetch_all(executor).await
    }

    /// 计数（与 `list_with_filters` 同一套筛选条件，不含排序与分页）。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        f: &PartListFilters<'_>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
            "SELECT COUNT(*) FROM t_part WHERE 1=1",
        );
        if !f.include_deleted { qb.push(" AND deleted_at IS NULL"); }
        if !f.customer_ids.is_empty() {
            qb.push(" AND customer_id = ANY(").push_bind(f.customer_ids.to_vec()).push(")");
        }
        if let Some(s) = f.status { qb.push(" AND status = ").push_bind(s.to_string()); }
        if !f.statuses.is_empty() {
            let arr = f.statuses.to_vec();
            if arr.len() == 1 {
                qb.push(" AND status = ").push_bind(arr[0].clone());
            } else {
                qb.push(" AND status = ANY(").push_bind(arr).push(")");
            }
        }
        if let Some(u) = f.is_urgent { qb.push(" AND is_urgent = ").push_bind(u); }
        if let Some(k) = f.keyword {
            let pat = format!("%{}%", k.trim());
            qb.push(" AND (name ILIKE ").push_bind(pat.clone())
              .push(" OR drawing_no ILIKE ").push_bind(pat.clone())
              .push(" OR serial_no ILIKE ").push_bind(pat)
              .push(")");
        }
        let row: (i64,) = qb.build_query_as().fetch_one(executor).await?;
        Ok(row.0)
    }

    /// 装配件的子件列表（service `get_assembly` 详情接口用）。
    ///
    /// 与既有 `list_children` 语义相同，但**不**走 sqlx 宏（避免 SELECT * 与 TPart
    /// 字段顺序耦合），用 `query_as` + 显式列清单；`include_deleted=false` 时过滤软删。
    /// `serial_no` 升序以稳定 children 顺序（与服务端 `{asm_serial}-{i:02d}` 一致）。
    pub async fn list_by_assembly_id<'e, E: PgExecutor<'e>>(
        executor: E,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        let sql = if include_deleted {
            "SELECT id, serial_no, name, drawing_no, applicant_name, quantity, \
             request_date, planned_delivery_date, actual_delivery_date, \
             customer_id, assembly_id, status, location, is_urgent, \
             current_holder_id, placed_at, next_process_id, \
             order_no, system_delivery_date, note, has_been_repaired, \
             version, created_at, created_by, updated_at, updated_by, \
             deleted_at, delivery_note_id \
             FROM t_part WHERE assembly_id = $1 \
             ORDER BY serial_no ASC NULLS LAST, id ASC"
        } else {
            "SELECT id, serial_no, name, drawing_no, applicant_name, quantity, \
             request_date, planned_delivery_date, actual_delivery_date, \
             customer_id, assembly_id, status, location, is_urgent, \
             current_holder_id, placed_at, next_process_id, \
             order_no, system_delivery_date, note, has_been_repaired, \
             version, created_at, created_by, updated_at, updated_by, \
             deleted_at, delivery_note_id \
             FROM t_part WHERE assembly_id = $1 AND deleted_at IS NULL \
             ORDER BY serial_no ASC NULLS LAST, id ASC"
        };
        sqlx::query_as::<_, TPart>(sql)
            .bind(assembly_id)
            .fetch_all(executor)
            .await
    }

    /// 子件随装配体一起建档；10 个参数都是必填的（id/asm_id/serial 是 3 个主键字段，
    /// name/quantity/drawing_no/planned_delivery_date/customer_id 是 5 个属性，created_by 是审计字段），
    /// 没有聚合语义，builder 包装反而是噪音。直接放宽即可。
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_child_for_assembly<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        customer_id: i64,
        assembly_id: i64,
        serial_no: &str,
        name: &str,
        drawing_no: Option<&str>,
        quantity: i32,
        planned_delivery_date: Option<chrono::NaiveDate>,
        current_user_id: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO t_part (
                id, name, drawing_no, applicant_name, quantity, request_date,
                planned_delivery_date, is_urgent, customer_id, assembly_id,
                order_no, system_delivery_date, note, status, location,
                unit_price, total_price, serial_no, version, created_by
            ) VALUES (
                $1, $2, $3, '', $4, NULL,
                $5, FALSE, $6, $7,
                NULL, NULL, NULL, 'PENDING', 'OFFICE',
                0, 0, $8, 0, $9
            )
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(drawing_no)
        .bind(quantity)
        .bind(planned_delivery_date)
        .bind(customer_id)
        .bind(assembly_id)
        .bind(serial_no)
        .bind(current_user_id)
        .execute(executor)
        .await?;
        Ok(())
    }
}