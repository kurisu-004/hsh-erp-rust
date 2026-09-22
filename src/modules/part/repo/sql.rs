//! part 域 SQL 真源（2026-09-22 D-6 重构对齐 iam / shelf / customer / part_batch 范本）
//!
//! 本文件合并原 `repo/part.rs` + `repo/batch.rs` + `repo/event.rs` 三文件 SQL，
//! 全文搬运、**内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//!
//! ZST struct `PartRepo` 收 `impl PgExecutor<'_>` 形参（与原 `repo/part.rs` 等
//! 一致），与胖 trait `PartRepoTrait` 方法 1:1 对应：
//!
//! - `PartRepo::get_by_id` ↔ `PartRepoTrait::get_by_id`
//! - `PartRepo::create_part` ↔ `PartRepoTrait::create_part`
//! - `PartRepo::find_batch_by_id` ↔ `PartRepoTrait::find_batch_by_id`
//! - `PartRepo::mark_batch_passed_inspection` ↔ `PartRepoTrait::mark_batch_passed_inspection`
//! - `PartRepo::insert_part_event` ↔ `PartRepoTrait::insert_part_event`
//! - ...
//!
//! Cross-module 调用方（delivery_note / assembly / outsource / part_file / statistics /
//! shelf 6 域，prod::worker_pool 1 域）继续走 `PartRepo::xxx(&mut *conn, ...)`
// ZST 静态调用形态——保持 12 处静态调用零修改（见 `repo/mod.rs` 注释详述）。

// ====== 原 `repo/part.rs` 全文搬迁 ======
//
// 5 个只读查询（get_by_id / list_by_ids / get_by_serial / list_children /
// get_part_inspected）+ 6 个 Phase PR-CRUD 方法（get_part_detail / create_part /
// update_part / soft_delete_part / list_with_filters / count_with_filters）+
// 3 个 assembly 子件方法（list_by_assembly_id / insert_child_for_assembly /
// cascade_sync_from_assembly / scale_children_quantity）+ 3 个 rollup 方法
//（get_part_rollup_state / update_part_rollup / clear_part_serial_no_when_completed）。

use sqlx::{PgConnection, PgExecutor};

use crate::modules::part::model::{TPart, TPartInspected};

/// part 域 ZST struct（承载 `t_part` + `t_part_batch` + `t_part_event` 三表全部
/// 固有静态方法）。
///
/// 跨模块调用方（delivery_note / assembly / outsource / part_file / statistics /
/// shelf / prod::worker_pool 共 7 域）继续走 `PartRepo::xxx(&mut *conn, ...)`
/// 静态方法调用——本任务**不能**破坏 `part::repo::PartRepo` 作为 ZST 的对外身份。
pub struct PartRepo;

/// `create_part` 输入：service 层用 builder 模式注入。
///
/// `id` 由 caller 预生成雪花；`status` 初始为 `'PENDING'`；`next_process_id` /
/// `serial_no` / `deleted_at` / `version` 走 DB 默认或 `NULL`。
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
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `actual_delivery_date`（t_part
/// 列已删；实际交付日期由 t_part_event DELIVERED 事件派生，不接受手工改）。
pub struct PartUpdate<'a> {
    pub name: Option<&'a str>,
    pub drawing_no: Option<&'a str>,
    pub applicant_name: Option<&'a str>,
    pub quantity: Option<i32>,
    pub order_no: Option<&'a str>,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub note: Option<&'a str>,
    pub is_urgent: Option<bool>,
    pub updated_by: i64,
}

/// `list_with_filters` / `count_with_filters` 输入。
///
/// 排序字段用字符串映射到白名单列名（防 SQL 注入），方向仅接受 `ASC` / `DESC`。
/// `status` 与 `statuses` 互不冲突：service 层按业务场景二选一传入。
///
/// 2026-09-17 PR-4 守卫修复：新增 `locations` / `holder_ids` 两过滤项。
///
/// - `locations`：字符串白名单（OFFICE / PRODUCTION_SHELF / WORKER /
///   INSPECTION_SHELF / OUTSOURCE_COMPANY），查 `t_part_batch.location`。
/// - `holder_ids`：雪花 ID 列表，多态查 `t_part_batch.current_holder_id`（命中
///   t_shelf / t_worker / t_outsource_company 任一即算）。
///
/// 两者均通过 EXISTS 子查询挂到 `t_part`（PR-2 已删 location / current_holder_id
/// 列），按 part 下任一 active batch 命中即返。空切片 → 不过滤（与旧行为一致）。
#[derive(Debug, Default, Clone)]
pub struct PartListFilters<'a> {
    pub customer_ids: &'a [i64],
    pub status: Option<&'a str>,
    pub statuses: &'a [String],
    pub is_urgent: Option<bool>,
    pub keyword: Option<&'a str>,
    pub locations: &'a [String],
    pub holder_ids: &'a [i64],
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
                   request_date, planned_delivery_date,
                   customer_id, assembly_id, status,
                   is_urgent, next_process_id,
                   order_no, system_delivery_date, note,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, process_chain_id
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
                   request_date, planned_delivery_date,
                   customer_id, assembly_id, status,
                   is_urgent, next_process_id,
                   order_no, system_delivery_date, note,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, process_chain_id
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
                   request_date, planned_delivery_date,
                   customer_id, assembly_id, status,
                   is_urgent, next_process_id,
                   order_no, system_delivery_date, note,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, process_chain_id
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
                   request_date, planned_delivery_date,
                   customer_id, assembly_id, status,
                   is_urgent, next_process_id,
                   order_no, system_delivery_date, note,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, process_chain_id
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

    /// to_ship 流专用最小投影（to-XXX 重命名后的 PartOut 必需列）。
    ///
    /// `include_deleted = false`（to_ship 不应对软删件操作）。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：投影删 `actual_delivery_date` /
    /// `current_holder_id`（t_part 列已删）。
    pub async fn get_part_inspected<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<Option<TPartInspected>, sqlx::Error> {
        sqlx::query_as!(
            TPartInspected,
            r#"
            SELECT id, serial_no, name, drawing_no, status, version, quantity,
                   order_no, updated_at, updated_by
            FROM t_part
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            part_id,
        )
        .fetch_optional(executor)
        .await
    }

    // ===== Phase PR-CRUD 新增 =====

    /// 详情接口：完整 29 列行（含软删检测）。
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
                   request_date, planned_delivery_date,
                   customer_id, assembly_id, status,
                   is_urgent, next_process_id,
                   order_no, system_delivery_date, note,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, process_chain_id
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
            new.id,
            new.name,
            new.drawing_no,
            new.applicant_name,
            new.quantity,
            new.request_date,
            new.planned_delivery_date,
            new.is_urgent,
            new.customer_id,
            new.assembly_id,
            new.order_no,
            new.system_delivery_date,
            new.note,
            new.created_by,
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
        if let Some(v) = upd.name {
            qb.push(", name = ").push_bind(v.to_string());
        }
        if let Some(v) = upd.drawing_no {
            qb.push(", drawing_no = ").push_bind(v.to_string());
        }
        if let Some(v) = upd.applicant_name {
            qb.push(", applicant_name = ").push_bind(v.to_string());
        }
        if let Some(v) = upd.quantity {
            qb.push(", quantity = ").push_bind(v);
        }
        if let Some(v) = upd.order_no {
            qb.push(", order_no = ").push_bind(v.to_string());
        }
        if let Some(v) = upd.system_delivery_date {
            qb.push(", system_delivery_date = ").push_bind(v);
        }
        if let Some(v) = upd.planned_delivery_date {
            qb.push(", planned_delivery_date = ").push_bind(v);
        }
        if let Some(v) = upd.note {
            qb.push(", note = ").push_bind(v.to_string());
        }
        if let Some(v) = upd.is_urgent {
            qb.push(", is_urgent = ").push_bind(v);
        }
        qb.push(" WHERE id = ").push_bind(part_id);
        qb.push(" AND version = ").push_bind(expected_version);
        qb.push(" AND deleted_at IS NULL");
        let r = qb.build().execute(executor).await?;
        Ok(r.rows_affected())
    }

    /// 软删：`deleted_at = now()` + `version += 1`。
    ///
    /// 守卫：`status NOT IN ('DELIVERED','COMPLETED')` —— 已签收 / 已完结的
    /// 工单不允许软删，由 service 层根据返回行数判断并映射错误码。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：t_part.delivery_note_id 列已删，
    /// 「已挂送货单禁删」守卫移出本 UPDATE，改由 service 层用
    /// `PartBatchRepo::has_active_batch_on_delivery_note` 预检（批次级真相源）。
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
              AND deleted_at IS NULL
            "#,
            part_id,
            expected_version,
            current_user_id,
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
    ///
    /// 2026-09-17 PR-4 守卫修复：`locations` / `holder_ids` 走 EXISTS 子查询
    /// 关联 `t_part_batch`（PR-2 已删 t_part.location / current_holder_id）。
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
        let order_dir = if f.sort_dir.eq_ignore_ascii_case("ASC") {
            "ASC"
        } else {
            "DESC"
        };

        let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
            "SELECT id, serial_no, name, drawing_no, applicant_name, quantity, \
                    request_date, planned_delivery_date, \
                    customer_id, assembly_id, status, is_urgent, \
                    next_process_id, \
                    order_no, system_delivery_date, note, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, process_chain_id \
             FROM t_part WHERE 1=1",
        );
        if !f.include_deleted {
            qb.push(" AND deleted_at IS NULL");
        }
        if !f.customer_ids.is_empty() {
            qb.push(" AND customer_id = ANY(")
                .push_bind(f.customer_ids.to_vec())
                .push(")");
        }
        if let Some(s) = f.status {
            qb.push(" AND status = ").push_bind(s.to_string());
        }
        if !f.statuses.is_empty() {
            let arr = f.statuses.to_vec();
            if arr.len() == 1 {
                qb.push(" AND status = ").push_bind(arr[0].clone());
            } else {
                qb.push(" AND status = ANY(").push_bind(arr).push(")");
            }
        }
        if let Some(u) = f.is_urgent {
            qb.push(" AND is_urgent = ").push_bind(u);
        }
        if let Some(k) = f.keyword {
            let pat = format!("%{}%", k.trim());
            qb.push(" AND (name ILIKE ")
                .push_bind(pat.clone())
                .push(" OR drawing_no ILIKE ")
                .push_bind(pat.clone())
                .push(" OR serial_no ILIKE ")
                .push_bind(pat)
                .push(")");
        }
        // 2026-09-17 PR-4 守卫修复：locations 查 t_part_batch.location（多值走 ANY）
        if !f.locations.is_empty() {
            qb.push(
                " AND EXISTS (SELECT 1 FROM t_part_batch pb \
                    WHERE pb.part_id = t_part.id \
                      AND pb.location = ANY(",
            )
            .push_bind(f.locations.to_vec())
            .push(
                ") \
                      AND pb.deleted_at IS NULL)",
            );
        }
        // 2026-09-17 PR-4 守卫修复：holder_ids 查 t_part_batch.current_holder_id（多值走 ANY；
        // 多态 holder：t_shelf / t_worker / t_outsource_company 任一匹配即命中同一雪花 id）
        if !f.holder_ids.is_empty() {
            qb.push(
                " AND EXISTS (SELECT 1 FROM t_part_batch pb \
                    WHERE pb.part_id = t_part.id \
                      AND pb.current_holder_id = ANY(",
            )
            .push_bind(f.holder_ids.to_vec())
            .push(
                ") \
                      AND pb.deleted_at IS NULL)",
            );
        }
        qb.push(format!(
            " ORDER BY {order_col} {order_dir} NULLS LAST, id DESC"
        ));
        qb.push(" LIMIT ").push_bind(f.limit);
        qb.push(" OFFSET ").push_bind(f.offset);
        qb.build_query_as::<TPart>().fetch_all(executor).await
    }

    /// 计数（与 `list_with_filters` 同一套筛选条件，不含排序与分页）。
    ///
    /// 2026-09-17 PR-4 守卫修复：同步加 `locations` / `holder_ids` 过滤。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        f: &PartListFilters<'_>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: sqlx::QueryBuilder<sqlx::Postgres> =
            sqlx::QueryBuilder::new("SELECT COUNT(*) FROM t_part WHERE 1=1");
        if !f.include_deleted {
            qb.push(" AND deleted_at IS NULL");
        }
        if !f.customer_ids.is_empty() {
            qb.push(" AND customer_id = ANY(")
                .push_bind(f.customer_ids.to_vec())
                .push(")");
        }
        if let Some(s) = f.status {
            qb.push(" AND status = ").push_bind(s.to_string());
        }
        if !f.statuses.is_empty() {
            let arr = f.statuses.to_vec();
            if arr.len() == 1 {
                qb.push(" AND status = ").push_bind(arr[0].clone());
            } else {
                qb.push(" AND status = ANY(").push_bind(arr).push(")");
            }
        }
        if let Some(u) = f.is_urgent {
            qb.push(" AND is_urgent = ").push_bind(u);
        }
        if let Some(k) = f.keyword {
            let pat = format!("%{}%", k.trim());
            qb.push(" AND (name ILIKE ")
                .push_bind(pat.clone())
                .push(" OR drawing_no ILIKE ")
                .push_bind(pat.clone())
                .push(" OR serial_no ILIKE ")
                .push_bind(pat)
                .push(")");
        }
        if !f.locations.is_empty() {
            qb.push(
                " AND EXISTS (SELECT 1 FROM t_part_batch pb \
                    WHERE pb.part_id = t_part.id \
                      AND pb.location = ANY(",
            )
            .push_bind(f.locations.to_vec())
            .push(
                ") \
                      AND pb.deleted_at IS NULL)",
            );
        }
        if !f.holder_ids.is_empty() {
            qb.push(
                " AND EXISTS (SELECT 1 FROM t_part_batch pb \
                    WHERE pb.part_id = t_part.id \
                      AND pb.current_holder_id = ANY(",
            )
            .push_bind(f.holder_ids.to_vec())
            .push(
                ") \
                      AND pb.deleted_at IS NULL)",
            );
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
             request_date, planned_delivery_date, \
             customer_id, assembly_id, status, is_urgent, \
             next_process_id, \
             order_no, system_delivery_date, note, \
             version, created_at, created_by, updated_at, updated_by, \
             deleted_at, process_chain_id \
             FROM t_part WHERE assembly_id = $1 \
             ORDER BY serial_no ASC NULLS LAST, id ASC"
        } else {
            "SELECT id, serial_no, name, drawing_no, applicant_name, quantity, \
             request_date, planned_delivery_date, \
             customer_id, assembly_id, status, is_urgent, \
             next_process_id, \
             order_no, system_delivery_date, note, \
             version, created_at, created_by, updated_at, updated_by, \
             deleted_at, process_chain_id \
             FROM t_part WHERE assembly_id = $1 AND deleted_at IS NULL \
             ORDER BY serial_no ASC NULLS LAST, id ASC"
        };
        sqlx::query_as::<_, TPart>(sql)
            .bind(assembly_id)
            .fetch_all(executor)
            .await
    }

    /// 子件随装配体一起建档；除 3 个主键（id / assembly_id / serial_no）、
    /// 5 个子件自身属性（name / drawing_no / quantity / planned_delivery_date /
    /// customer_id）和 1 个审计字段（created_by）外，新增 6 个**继承自父件**
    /// 的字段（`inherit`），按 `refactor-part-assembly-batch.md §3.1`（2026-09-11）
    /// 实施：子件从父件 `t_assembly` 继承 `applicant_name` / `request_date` /
    /// `order_no` / `system_delivery_date` / `is_urgent` / `note`，
    /// `planned_delivery_date` 由 service 层在传入前按「子件入参优先，缺省继承
    /// 父件」完成合并。
    ///
    /// 2026-09-11 part/assembly/batch 重构方案 §4.1 (PR-B1)：本函数额外插入
    /// 一条 `batch_no=1 / status='PENDING' / location='OFFICE' / quantity=$quantity`
    /// 的初始批次（part/assembly/batch 重构后所有车间流转都锚定 batch，必须有
    /// 初始批次）。`initial_batch_id` 由 caller 预生成雪花。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：子件 part 行不再写
    /// `location='OFFICE'`（t_part.location 列已删）；批次行仍写
    /// `location='OFFICE'`（位置信息真相源在 t_part_batch）。
    ///
    /// 函数签名收 `&mut PgConnection`（非 `impl PgExecutor<'_>`），因为要在同一
    /// 事务内连发两条 INSERT（与 `split_batch_for_partial_pass` / `split_batch`
    /// 同模式）。
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_child_for_assembly(
        conn: &mut sqlx::PgConnection,
        id: i64,
        customer_id: i64,
        assembly_id: i64,
        serial_no: &str,
        name: &str,
        drawing_no: Option<&str>,
        quantity: i32,
        planned_delivery_date: Option<chrono::NaiveDate>,
        inherit: ChildInheritFields<'_>,
        current_user_id: i64,
        initial_batch_id: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_part (
                id, name, drawing_no, applicant_name, quantity, request_date,
                planned_delivery_date, is_urgent, customer_id, assembly_id,
                order_no, system_delivery_date, note, status,
                unit_price, total_price, serial_no, version, created_by
            ) VALUES (
                $1, $2, $3, $4, $5, $6,
                $7, $8, $9, $10,
                $11, $12, $13, 'PENDING',
                0, 0, $14, 0, $15
            )
            "#,
            id,
            name,
            drawing_no,
            inherit.applicant_name,
            quantity,
            inherit.request_date,
            planned_delivery_date,
            inherit.is_urgent,
            customer_id,
            assembly_id,
            inherit.order_no,
            inherit.system_delivery_date,
            inherit.note,
            serial_no,
            current_user_id,
        )
        .execute(&mut *conn)
        .await?;
        // 初始批次（part/assembly/batch 重构方案 §4.1 PR-B1）：子件 location='OFFICE'。
        // 复用 `PartBatchRepo::create_initial_batch` 的 INSERT 形状，保持与
        // `create_part` / `batch_create_parts` 两个入口的批次初始化语义一致。
        //
        // 2026-09-16 PR-3 批次 step 化（migration 028）：
        // - 删 `next_process_id` / `placed_at` 列写入
        // - `current_process_step_id = NULL`（初始 batch 不在生产流）
        sqlx::query!(
            r#"
            INSERT INTO t_part_batch (
                id, part_id, batch_no, quantity, status, location,
                current_holder_id, current_process_step_id,
                delivery_note_id, parent_batch_id,
                version, created_at, created_by, updated_at, updated_by
            ) VALUES (
                $1, $2, 1, $3, 'PENDING', 'OFFICE',
                NULL, NULL,
                NULL, NULL,
                0, now(), $4, now(), $4
            )
            "#,
            initial_batch_id,
            id,
            quantity,
            current_user_id,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
}

/// 子件从父装配件继承的字段包（§3.1）。
///
/// `applicant_name` 在 `t_part` 是 `NOT NULL VARCHAR(50)`，父可空 → service 层
/// 在传入前已用空串 `""` 兜底；`request_date` 在 `t_part` 也是 `NOT NULL`，
/// 由父件 `request_date`（可能为父 service 层默认今天）兜底。
/// `is_urgent` 父可空 → `bool` 缺省 `false` 由父 service 决定。
#[derive(Debug, Clone)]
pub struct ChildInheritFields<'a> {
    pub applicant_name: &'a str,
    pub request_date: chrono::NaiveDate,
    pub order_no: Option<&'a str>,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    pub note: Option<&'a str>,
}

impl PartRepo {
    /// §3.2 — 把父装配件"更新后的当前行值"覆盖级联到所有未软删子件。
    ///
    /// 同步字段：`request_date` / `applicant_name` / `order_no` /
    /// `system_delivery_date` / `planned_delivery_date` / `is_urgent` / `note` /
    /// `customer_id`。
    ///
    /// 排除：
    /// - `quantity`：单独走 §3.3 缩放逻辑。
    ///
    /// （2026-09-16 PR-2 注：原「排除 actual_delivery_date」条目随 migration 027
    /// 删列而消失 —— t_part 已无该列。）
    ///
    /// 实现上按"父件更新后的当前行值"做覆盖（即所有 8 个字段无条件覆写），
    /// 未变更字段被覆写为原值（语义无差），避免三态解析歧义。
    ///
    /// 返回受影响行数（含 version+1）。
    #[allow(clippy::too_many_arguments)]
    pub async fn cascade_sync_from_assembly<'e, E: PgExecutor<'e>>(
        executor: E,
        assembly_id: i64,
        request_date: chrono::NaiveDate,
        applicant_name: &str,
        order_no: Option<&str>,
        system_delivery_date: Option<chrono::NaiveDate>,
        planned_delivery_date: chrono::NaiveDate,
        is_urgent: bool,
        note: Option<&str>,
        customer_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r#"
            UPDATE t_part
            SET request_date = $2,
                applicant_name = $3,
                order_no = $4,
                system_delivery_date = $5,
                planned_delivery_date = $6,
                is_urgent = $7,
                note = $8,
                customer_id = $9,
                version = version + 1,
                updated_at = NOW(),
                updated_by = $10
            WHERE assembly_id = $1
              AND deleted_at IS NULL
            "#,
        )
        .bind(assembly_id)
        .bind(request_date)
        .bind(applicant_name)
        .bind(order_no)
        .bind(system_delivery_date)
        .bind(planned_delivery_date)
        .bind(is_urgent)
        .bind(note)
        .bind(customer_id)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }

    /// §3.3 — 父件 quantity 变化时，按比例缩放所有未软删子件的 quantity。
    ///
    /// 公式：`new_child_qty = max(1, round(child_qty * new_qty / old_qty))`；
    /// 单条 UPDATE 走 `GREATEST(1, ROUND(quantity::numeric * new_qty / old_qty))`
    /// 完成整批缩放，`version += 1`。
    ///
    /// 防御：`old_qty <= 0` → 直接跳过（视为无缩放），防除零 / 反向缩放。
    /// **不**追溯调整 `t_part_batch.quantity`（已拆分流转中的批次保持原量）。
    ///
    /// 返回受影响行数。
    pub async fn scale_children_quantity<'e, E: PgExecutor<'e>>(
        executor: E,
        assembly_id: i64,
        old_qty: i32,
        new_qty: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        if old_qty <= 0 || new_qty <= 0 {
            return Ok(0);
        }
        let res = sqlx::query(
            r#"
            UPDATE t_part
            SET quantity = GREATEST(1, ROUND(quantity::numeric * $2::numeric / $1::numeric)::int),
                version = version + 1,
                updated_at = NOW(),
                updated_by = $4
            WHERE assembly_id = $3
              AND deleted_at IS NULL
            "#,
        )
        .bind(old_qty)
        .bind(new_qty)
        .bind(assembly_id)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }
}

/// §3.3 — 套数缩放公式的纯函数表达（与 `scale_children_quantity` SQL
/// `GREATEST(1, ROUND(quantity::numeric * new_qty / old_qty))` 必须严格一致）。
///
/// 返回 `Some(new_child_qty)`；`old_qty <= 0` 或 `new_qty <= 0` → `None`
/// （service 层在这种情况跳过缩放，避免除零 / 反向缩放）。
///
/// 约定：
/// - 四舍五入方向：half-away-from-zero（与 PostgreSQL `ROUND(numeric)` 默认一致）。
/// - 下限：1（与 SQL `GREATEST(1, ...)` 一致）。
/// - 不追溯调整 `t_part_batch.quantity`（D1 决策）。
pub fn scale_qty(child_qty: i32, old_qty: i32, new_qty: i32) -> Option<i32> {
    if old_qty <= 0 || new_qty <= 0 {
        return None;
    }
    let raw = (child_qty as f64) * (new_qty as f64) / (old_qty as f64);
    let rounded = raw.round() as i32;
    Some(rounded.max(1))
}

#[cfg(test)]
mod tests {
    use super::scale_qty;

    /// 整数倍缩放：child=3, old=1, new=2 → round(6.0)=6
    #[test]
    fn scale_qty_integer_multiple() {
        assert_eq!(scale_qty(3, 1, 2), Some(6));
        assert_eq!(scale_qty(5, 1, 2), Some(10));
        assert_eq!(scale_qty(4, 2, 4), Some(8));
    }

    /// 四舍五入边界：child=1, old=3, new=5 → 1*5/3 ≈ 1.666 → round=2
    #[test]
    fn scale_qty_rounds_half_away_from_zero() {
        // 1.666... → 2
        assert_eq!(scale_qty(1, 3, 5), Some(2));
        // 2.666... → 3
        assert_eq!(scale_qty(2, 3, 4), Some(3));
        // 1.5 → 2（half away from zero）
        assert_eq!(scale_qty(3, 2, 1), Some(2));
        // 0.5 → 1（half away from zero，命中下限 1）
        assert_eq!(scale_qty(1, 2, 1), Some(1));
    }

    /// 下限 1：child_qty=1, old=10, new=3 → 0.3 → round=0 → GREATEST 1 = 1
    #[test]
    fn scale_qty_clamps_to_one() {
        assert_eq!(scale_qty(1, 10, 3), Some(1));
        assert_eq!(scale_qty(1, 100, 1), Some(1));
        // child=0 不可能（service 层会校验），但 formula 仍要正确处理
        assert_eq!(scale_qty(0, 10, 3), Some(1));
    }

    /// 防御：old_qty <= 0 / new_qty <= 0 → None（跳过缩放）
    #[test]
    fn scale_qty_skips_on_non_positive() {
        assert_eq!(scale_qty(3, 0, 2), None, "old_qty=0 应跳过");
        assert_eq!(scale_qty(3, -1, 2), None, "old_qty=-1 应跳过");
        assert_eq!(scale_qty(3, 1, 0), None, "new_qty=0 应跳过");
        assert_eq!(scale_qty(3, 1, -2), None, "new_qty=-2 应跳过");
    }
}

// ===== rollup 相关 repo 函数（从 refactor/part-assembly-Bchain 合并过来） =====
//
// 这些函数服务于 PR-B2 `PartService::sync_from_batch_change`（part 状态由 batch 变化
// rollup 派生）。单独一组 `impl PartRepo { ... }`，与上面的方向 A 级联 / 缩放函数区分。
//
// 2026-09-16 PR-2 瘦身（migration 027）：`t_part` 不再持有 `location` /
// `current_holder_id` / `placed_at` / `has_been_repaired`，rollup 只物化
// `status` + `next_process_id`（作为 part 派生列读缓存）。`get_part_rollup_state`
// 与 `update_part_rollup` 投影同步收窄；`mark_part_repairing_flag_only` 整体
// 删除（t_part 已无该列；返修事实由 t_part_event REPAIR_STARTED 事件追溯）。
//
// 2026-09-16 PR-3 批次 step 化（migration 028）：rollup 派生规则不变，
// `t_part.next_process_id` 仍保留为派生缓存，但**派生源**改为
// `min-progress 活跃 batch.current_process_step_id JOIN t_process_chain_step.process_id`。
// `compute_part_target` 在 service 层读 `BatchForRollup` 时同步改为读 step_id
// （详见 `part/service/rollup.rs::sync_from_batch_change` 的派生逻辑）。
// `t_part_batch.next_process_id` 列已删，rollup 取 `process_id` 需经 step JOIN。
//
// 注：本块位于 `mod tests` 之后，触发 clippy::items_after_test_module 警告。
// 与 `worker_scan.rs::needless_late_init` 同类 pre-existing 例外，合并期不便重构
// impl 块布局，加 `#[allow]` 豁免。
#[allow(clippy::items_after_test_module)]
impl PartRepo {
    /// rollup 读侧：part 当前派生列投影（PR-B2 `sync_from_batch_change` 用）。
    ///
    /// 2026-09-16 PR-2 瘦身：2 列投影（status / next_process_id），其它派
    /// 生列真相源在 t_part_batch。
    /// `None` 表示 part 不存在或已软删。
    pub async fn get_part_rollup_state<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<Option<crate::modules::part::model::TPartRollupState>, sqlx::Error> {
        sqlx::query_as!(
            crate::modules::part::model::TPartRollupState,
            r#"
            SELECT status, next_process_id
            FROM t_part
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            part_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// rollup 写侧：派生列 UPDATE（PR-B2 `sync_from_batch_change` 用）。
    ///
    /// 2026-09-16 PR-2 瘦身：只写 `status` + `next_process_id`（其它列已从
    /// `t_part` 删除，真相源在 `t_part_batch`）。**不走 OCC**（派生写）：
    /// `WHERE id=$1 AND deleted_at IS NULL`。并发 rollup 由 SQL 行锁串行化；
    /// `version += 1` 仍写入（保证审计字段单调）。0 行 → part 已被并发软删
    /// （防御性 caller 走 NoChange）。
    pub async fn update_part_rollup<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        status: &str,
        next_process_id: Option<i64>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"
            UPDATE t_part
            SET status          = $2,
                next_process_id = $3,
                version         = version + 1,
                updated_at      = now(),
                updated_by      = $4
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            part_id,
            status,
            next_process_id,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 2026-09-11 part/assembly/batch 重构方案 §4.3 (PR-B3) complete 辅助：
    /// part 进入 COMPLETED 时清空 `serial_no`（序列号已转交送货单）。
    ///
    /// 条件：`status='COMPLETED'`（rollup 已把 part 推到终态）。0 行不影响事务。
    /// **不走 OCC**（衍生写）。
    pub async fn clear_part_serial_no_when_completed<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            r#"
            UPDATE t_part
            SET serial_no  = NULL,
                version    = version + 1,
                updated_at = now(),
                updated_by = $2
            WHERE id = $1 AND status = 'COMPLETED' AND deleted_at IS NULL
            "#,
        )
        .bind(part_id)
        .bind(updated_by)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }
}

// ====== 原 `repo/batch.rs` 全文搬迁 ======
//
// 3 个 `find_*`（inprocess / scan_target / inspection_for_fail） +
// 1 个 `find_current_inspection_batch_id`（复用 find_inprocess_batch_for_part）+
// 1 个 `find_batch_by_id`（按 id 定位无 status 守卫）+
// 1 个 `find_inprocess_batch_by_id_and_holder`（worker-pool 用）+
// 1 个 `find_worker_held_batch_for_part`（worker-scan 用）+
// 1 个 `split_batch_for_partial_pass`（薄包装 PartBatchRepo::_split_batch_inner）+
// 3 个 `mark_*_inspection`（passed / inspected / failed）+
// 2 个 `mark_*_worker_pool`（returned / find_by_id_and_holder）+
// 8 个 `mark_*_lifecycle`（delivered / completed / part_cancelled /
// batch_cancelled / repairing / cancel_all_active_batches_for_part / clear_serial）。

use crate::modules::part_batch::model::TPartBatch;

impl PartRepo {
    /// 定位 part 的 INSPECTION 状态批次。
    ///
    /// - `expected_batch_id = None`：先 COUNT 校验唯一性（≥2 → 歧义 `RowNotFound`），
    ///   == 0 → `Ok(None)`，== 1 → 取 id 最小者。
    /// - `expected_batch_id = Some(bid)`：按 id 校验 ownership。
    ///
    /// 签名收 `&mut PgConnection`：方法在 `None` 分支需在同一事务内连发两条 SQL。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加 current_process_step_id。
    pub async fn find_inprocess_batch_for_part(
        conn: &mut PgConnection,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        match expected_batch_id {
            Some(bid) => {
                sqlx::query_as!(
                    TPartBatch,
                    r#"
                SELECT id, part_id, batch_no, quantity, status, location,
                       current_holder_id, current_process_step_id,
                       delivery_note_id, parent_batch_id,
                       version, created_at, created_by, updated_at, updated_by,
                       deleted_at
                FROM t_part_batch
                WHERE id = $1 AND part_id = $2 AND status = 'INSPECTION'
                  AND deleted_at IS NULL
                "#,
                    bid,
                    part_id,
                )
                .fetch_optional(&mut *conn)
                .await
            }
            None => {
                let count: i64 = sqlx::query_scalar!(
                    r#"
                    SELECT COUNT(*) AS "n!"
                    FROM t_part_batch
                    WHERE part_id = $1 AND status = 'INSPECTION' AND deleted_at IS NULL
                    "#,
                    part_id,
                )
                .fetch_one(&mut *conn)
                .await?;
                match count {
                    0 => Ok(None),
                    1 => {
                        sqlx::query_as!(
                            TPartBatch,
                            r#"
                        SELECT id, part_id, batch_no, quantity, status, location,
                               current_holder_id, current_process_step_id,
                               delivery_note_id, parent_batch_id,
                               version, created_at, created_by, updated_at, updated_by,
                               deleted_at
                        FROM t_part_batch
                        WHERE part_id = $1 AND status = 'INSPECTION' AND deleted_at IS NULL
                        ORDER BY id ASC
                        LIMIT 1
                        "#,
                            part_id,
                        )
                        .fetch_optional(&mut *conn)
                        .await
                    }
                    _ => Err(sqlx::Error::RowNotFound),
                }
            }
        }
    }

    /// 定位 to-inspection 的目标批次（白名单 `{PENDING, PROGRAMMING, IN_PROCESS}`）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加 current_process_step_id。
    pub async fn find_scan_target_batch(
        conn: &mut PgConnection,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        match expected_batch_id {
            Some(bid) => {
                sqlx::query_as!(
                    TPartBatch,
                    r#"
                SELECT id, part_id, batch_no, quantity, status, location,
                       current_holder_id, current_process_step_id,
                       delivery_note_id, parent_batch_id,
                       version, created_at, created_by, updated_at, updated_by,
                       deleted_at
                FROM t_part_batch
                WHERE id = $1 AND part_id = $2
                  AND status IN ('PENDING', 'PROGRAMMING', 'IN_PROCESS')
                  AND deleted_at IS NULL
                "#,
                    bid,
                    part_id,
                )
                .fetch_optional(&mut *conn)
                .await
            }
            None => {
                let count: i64 = sqlx::query_scalar!(
                    r#"
                    SELECT COUNT(*) AS "n!"
                    FROM t_part_batch
                    WHERE part_id = $1
                      AND status IN ('PENDING', 'PROGRAMMING', 'IN_PROCESS')
                      AND deleted_at IS NULL
                    "#,
                    part_id,
                )
                .fetch_one(&mut *conn)
                .await?;
                match count {
                    0 => Ok(None),
                    1 => {
                        sqlx::query_as!(
                            TPartBatch,
                            r#"
                        SELECT id, part_id, batch_no, quantity, status, location,
                               current_holder_id, current_process_step_id,
                               delivery_note_id, parent_batch_id,
                               version, created_at, created_by, updated_at, updated_by,
                               deleted_at
                        FROM t_part_batch
                        WHERE part_id = $1
                          AND status IN ('PENDING', 'PROGRAMMING', 'IN_PROCESS')
                          AND deleted_at IS NULL
                        ORDER BY id ASC
                        LIMIT 1
                        "#,
                            part_id,
                        )
                        .fetch_optional(&mut *conn)
                        .await
                    }
                    _ => Err(sqlx::Error::RowNotFound),
                }
            }
        }
    }

    /// 定位 to-process 的目标 INSPECTION 批次。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加 current_process_step_id。
    pub async fn find_inspection_batch_for_fail(
        conn: &mut PgConnection,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        match expected_batch_id {
            Some(bid) => {
                sqlx::query_as!(
                    TPartBatch,
                    r#"
                SELECT id, part_id, batch_no, quantity, status, location,
                       current_holder_id, current_process_step_id,
                       delivery_note_id, parent_batch_id,
                       version, created_at, created_by, updated_at, updated_by,
                       deleted_at
                FROM t_part_batch
                WHERE id = $1 AND part_id = $2 AND status = 'INSPECTION'
                  AND deleted_at IS NULL
                "#,
                    bid,
                    part_id,
                )
                .fetch_optional(&mut *conn)
                .await
            }
            None => {
                let count: i64 = sqlx::query_scalar!(
                    r#"
                    SELECT COUNT(*) AS "n!"
                    FROM t_part_batch
                    WHERE part_id = $1 AND status = 'INSPECTION' AND deleted_at IS NULL
                    "#,
                    part_id,
                )
                .fetch_one(&mut *conn)
                .await?;
                match count {
                    0 => Ok(None),
                    1 => {
                        sqlx::query_as!(
                            TPartBatch,
                            r#"
                        SELECT id, part_id, batch_no, quantity, status, location,
                               current_holder_id, current_process_step_id,
                               delivery_note_id, parent_batch_id,
                               version, created_at, created_by, updated_at, updated_by,
                               deleted_at
                        FROM t_part_batch
                        WHERE part_id = $1 AND status = 'INSPECTION' AND deleted_at IS NULL
                        ORDER BY id ASC
                        LIMIT 1
                        "#,
                            part_id,
                        )
                        .fetch_optional(&mut *conn)
                        .await
                    }
                    _ => Err(sqlx::Error::RowNotFound),
                }
            }
        }
    }

    /// 取 part 当前活跃 INSPECTION 批次的 id（前端轮询用）。
    ///
    /// 复用 [`find_inprocess_batch_for_part`] 的 None 路径（自动 COUNT
    /// 校验唯一性）；仅当恰好 1 条 INSPECTION 批次时返回 Some(id)，其它
    /// 情形（含 0 条 / ≥2 条歧义）返回 None —— 前端轮询接口对此宽容即可。
    pub async fn find_current_inspection_batch_id(
        conn: &mut PgConnection,
        part_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        Ok(Self::find_inprocess_batch_for_part(conn, part_id, None)
            .await?
            .map(|b| b.id))
    }

    /// 按 id + 未软删定位 `t_part_batch` 行。
    ///
    /// 与 `find_inprocess_batch_for_part` / `find_scan_target_batch` 等的差异：
    /// 本方法不限制 `status` / `part_id`，caller 拿到 `TPartBatch` 后用
    /// `part_id` / `status` 字段自行派发。常用于批量端点的 item 反查
    /// （按 batch_id 拿 part_id，再调对应的 `to_*_core`）。
    ///
    /// 不存在或已软删 → `Ok(None)`，由 service 层映射
    /// `20109 BIZ_PART_BATCH_NOT_FOUND`。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加 current_process_step_id。
    pub async fn find_batch_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_batch
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            batch_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 批量通过（OCC UPDATE）。
    pub async fn mark_batch_passed_inspection<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET status     = 'READY_TO_SHIP',
                version    = version + 1,
                updated_at = now(),
                updated_by = $3
            WHERE id = $1 AND version = $2 AND status = 'INSPECTION'
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// to-inspection 第一步：批次状态同步（OCC UPDATE t_part_batch）。
    pub async fn mark_batch_inspected<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET status            = 'INSPECTION',
                location          = 'INSPECTION_SHELF',
                current_holder_id = $3,
                -- 2026-09-16 PR-3：to_inspection 第一步保留 current_process_step_id
                -- （即被打回的那一步，让 INSPECTION→to_process 时不丢 step 上下文）
                version           = version + 1,
                updated_at        = now(),
                updated_by        = $4
            WHERE id = $1 AND version = $2
              AND status IN ('PENDING', 'PROGRAMMING', 'IN_PROCESS')
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            shelf_id,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// to-process：批次打回生产架（OCC UPDATE t_part_batch）。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：
    /// - 参数 `next_process_id: i64` 改 `current_process_step_id: Option<i64>`
    /// - 写入列：t_part_batch.next_process_id（已删）→ t_part_batch.current_process_step_id
    /// - step_id 由 phase1 service 在调用本函数前按 `chain_id + process_id` 解析后传入
    pub async fn mark_batch_failed_inspection<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET status                  = 'IN_PROCESS',
                location                = 'PRODUCTION_SHELF',
                current_holder_id       = $3,
                current_process_step_id = $4,
                version                 = version + 1,
                updated_at              = now(),
                updated_by              = $5
            WHERE id = $1 AND version = $2 AND status = 'INSPECTION'
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            shelf_id,
            current_process_step_id,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// worker-pool admin_remove 用：按 `id + current_holder_id` 定位 IN_PROCESS+WORKER 批次。
    ///
    /// 必须满足：`status='IN_PROCESS'` + `location='WORKER'` + `current_holder_id = holder_id`，
    /// 且 `deleted_at IS NULL`。
    /// 0 行 / 不命中 → `Ok(None)`，由 service 层映射 `20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER`。
    ///
    /// 签名收 `&mut PgConnection`（同 `find_inprocess_batch_for_part`）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加 current_process_step_id。
    pub async fn find_inprocess_batch_by_id_and_holder(
        conn: &mut PgConnection,
        batch_id: i64,
        holder_id: i64,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at
            FROM t_part_batch
            WHERE id = $1 AND current_holder_id = $2
              AND status = 'IN_PROCESS' AND location = 'WORKER'
              AND deleted_at IS NULL
            "#,
            batch_id,
            holder_id,
        )
        .fetch_optional(&mut *conn)
        .await
    }

    /// worker-pool admin_remove / worker-scan RETURNED 用：批次 holder worker → shelf（OCC）。
    ///
    /// 0 行 → 40901 VERSION_CONFLICT / 状态非 IN_PROCESS / location 非 WORKER / 已软删
    ///   —— 由 service 层映射。
    /// 成功 → `current_holder_id = shelf_id`，`location = 'PRODUCTION_SHELF'`，
    ///   `current_process_step_id = $4`，`version += 1`。
    ///
    /// `current_user_id` 写入 `updated_by`（nullable 与既有路径一致）。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：
    /// - 参数 `next_process_id: i64` 改 `current_process_step_id: Option<i64>`
    /// - 写入列改为 t_part_batch.current_process_step_id
    pub async fn mark_batch_returned<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET current_holder_id       = $3,
                location                = 'PRODUCTION_SHELF',
                current_process_step_id = $4,
                version                 = version + 1,
                updated_at              = now(),
                updated_by              = $5
            WHERE id = $1 AND version = $2
              AND status = 'IN_PROCESS' AND location = 'WORKER'
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            shelf_id,
            current_process_step_id,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// 定位 worker 持有的 IN_PROCESS 批次（worker-scan 用）。
    ///
    /// 与 `find_inprocess_batch_for_part` 同形：
    /// - `expected_batch_id = Some(bid)`：按 id 校验 ownership
    ///   （part_id + current_holder_id + status='IN_PROCESS' + location='WORKER'）。
    /// - `expected_batch_id = None`：先 COUNT 校验唯一性
    ///   （≥2 → `RowNotFound`；== 0 → `Ok(None)`；== 1 → 取 id 最小者）。
    ///
    /// 唯一性守卫原因：worker 持有多个同 part_id 的 IN_PROCESS+WORKER 批次时，
    /// `ORDER BY id LIMIT 1` 静默取最小 id 可能选错批次。
    ///
    /// 签名收 `&mut PgConnection`：方法在 `None` 分支需在同一事务内连发两条 SQL
    /// （COUNT + SELECT），与 `find_inprocess_batch_for_part` 同形。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加 current_process_step_id。
    pub async fn find_worker_held_batch_for_part(
        conn: &mut PgConnection,
        part_id: i64,
        worker_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        match expected_batch_id {
            Some(bid) => {
                sqlx::query_as!(
                    TPartBatch,
                    r#"
                SELECT id, part_id, batch_no, quantity, status, location,
                       current_holder_id, current_process_step_id,
                       delivery_note_id, parent_batch_id,
                       version, created_at, created_by, updated_at, updated_by,
                       deleted_at
                FROM t_part_batch
                WHERE id = $1 AND part_id = $2 AND current_holder_id = $3
                  AND status = 'IN_PROCESS' AND location = 'WORKER'
                  AND deleted_at IS NULL
                "#,
                    bid,
                    part_id,
                    worker_id,
                )
                .fetch_optional(&mut *conn)
                .await
            }
            None => {
                let count: i64 = sqlx::query_scalar!(
                    r#"
                    SELECT COUNT(*) AS "n!"
                    FROM t_part_batch
                    WHERE part_id = $1 AND current_holder_id = $2
                      AND status = 'IN_PROCESS' AND location = 'WORKER'
                      AND deleted_at IS NULL
                    "#,
                    part_id,
                    worker_id,
                )
                .fetch_one(&mut *conn)
                .await?;
                match count {
                    0 => Ok(None),
                    1 => {
                        sqlx::query_as!(
                            TPartBatch,
                            r#"
                        SELECT id, part_id, batch_no, quantity, status, location,
                               current_holder_id, current_process_step_id,
                               delivery_note_id, parent_batch_id,
                               version, created_at, created_by, updated_at, updated_by,
                               deleted_at
                        FROM t_part_batch
                        WHERE part_id = $1 AND current_holder_id = $2
                          AND status = 'IN_PROCESS' AND location = 'WORKER'
                          AND deleted_at IS NULL
                        ORDER BY id ASC
                        LIMIT 1
                        "#,
                            part_id,
                            worker_id,
                        )
                        .fetch_optional(&mut *conn)
                        .await
                    }
                    // ≥2 个 IN_PROCESS+WORKER 批次：歧义。Service 层负责把
                    // `sqlx::Error::RowNotFound` 翻译为 `AppError::Biz` /
                    // `20114 / BIZ_PART_BATCH_NOT_HELD_BY_WORKER`。
                    _ => Err(sqlx::Error::RowNotFound),
                }
            }
        }
    }

    /// 部分通过：拆出新批次（status 由 `new_batch_status` 指定）。
    ///
    /// 原子化三步（共享事务）：
    /// 1. 算同 part_id 下下一个 `batch_no`（max + 1）
    /// 2. INSERT 新批次（quantity = split_quantity，status = `new_batch_status`）
    /// 3. UPDATE 源批次 `quantity -= split_quantity`（OCC + 数量守卫）
    ///
    /// `new_batch_status` 通常传源批次 status：
    /// - `to_ship` / `to_process`：源 = `INSPECTION`，新 = `INSPECTION`
    /// - `to_inspection`：源 ∈ `{PENDING, PROGRAMMING, IN_PROCESS}`，新 = 源 status
    ///   （确保 `mark_batch_inspected` 的 WHERE 守卫能匹配新批次）
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：删 `next_process_id` /
    /// `placed_at` 列写入；`current_process_step_id` 由内部 `_split_batch_inner`
    /// 走 SELECT 继承源。
    ///
    /// 2026-09-17 PR-4 卫生项 B2：薄包装委托到 `PartBatchRepo::_split_batch_inner`
    /// （part_batch/repo.rs）；`split_batch`（手动部分量）也委托同一 helper。
    #[allow(clippy::too_many_arguments)]
    pub async fn split_batch_for_partial_pass(
        conn: &mut PgConnection,
        new_batch_id: i64,
        src_batch_id: i64,
        src_version: i32,
        part_id: i64,
        split_quantity: i32,
        new_batch_status: &str,
        current_user_id: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        let user_id = current_user_id.unwrap_or(0);
        crate::modules::part_batch::repo::PartBatchRepo::_split_batch_inner(
            conn,
            new_batch_id,
            src_batch_id,
            src_version,
            part_id,
            split_quantity,
            new_batch_status,
            user_id,
        )
        .await
    }

    // ===== Phase PR-CRUD 新增：8 个 lifecycle mark_* =====

    /// 批次 READY_TO_SHIP → DELIVERED（OCC UPDATE t_part_batch）。
    pub async fn mark_batch_delivered<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part_batch SET status='DELIVERED', version=version+1,
                updated_at=now(), updated_by=$3
               WHERE id=$1 AND version=$2 AND status='READY_TO_SHIP' AND deleted_at IS NULL"#,
            batch_id,
            expected_version,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 批次 DELIVERED → COMPLETED。
    pub async fn mark_batch_completed<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part_batch SET status='COMPLETED', version=version+1,
                updated_at=now(), updated_by=$3
               WHERE id=$1 AND version=$2 AND status='DELIVERED' AND deleted_at IS NULL"#,
            batch_id,
            expected_version,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 工单取消（OCC UPDATE t_part）：白名单 5 状态（PENDING / PROGRAMMING /
    /// INSPECTION / READY_TO_SHIP / DELIVERED），清空 `serial_no`。
    pub async fn mark_part_cancelled<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part SET status='CANCELLED', version=version+1,
                updated_at=now(), updated_by=$3, serial_no=NULL
               WHERE id=$1 AND version=$2
                 AND status IN ('PENDING','PROGRAMMING','INSPECTION','READY_TO_SHIP','DELIVERED')
                 AND deleted_at IS NULL"#,
            part_id,
            expected_version,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 批次取消（OCC UPDATE t_part_batch）：白名单 5 状态。
    pub async fn mark_batch_cancelled<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part_batch SET status='CANCELLED', version=version+1,
                updated_at=now(), updated_by=$3
               WHERE id=$1 AND version=$2
                 AND status IN ('PENDING','PROGRAMMING','INSPECTION','READY_TO_SHIP','DELIVERED')
                 AND deleted_at IS NULL"#,
            batch_id,
            expected_version,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 批次 IN_PROCESS → REPAIRING。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：t_part_batch 删 `has_been_repaired`
    /// 列；返修事实由 `t_part_event` REPAIR_STARTED 事件追溯，本函数不再
    /// 写返修标。
    pub async fn mark_batch_repairing<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part_batch SET status='REPAIRING', version=version+1,
                updated_at=now(), updated_by=$3
               WHERE id=$1 AND version=$2 AND status='IN_PROCESS' AND deleted_at IS NULL"#,
            batch_id,
            expected_version,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 2026-09-11 part/assembly/batch 重构方案 §4.2 (PR-B2)：part cancel 时
    /// 级联取消**全部活跃批次**（不只「最近一条 source-status」）。
    ///
    /// 单条 UPDATE：`WHERE part_id = $1 AND deleted_at IS NULL` 把 part 下所有
    /// 活跃 batch → CANCELLED（不走 OCC；version += 1；写 updated_by）。
    /// 不在 SQL 上做 status 白名单过滤：cancel 5 状态白名单由 service 层
    /// `can_transition_to` 守；此处只管「part 已决定 cancel，批量同步 batch」。
    ///
    /// 返回影响行数（0 表示 part 下无活跃批次 —— 不视为错误，由 caller 决定）。
    pub async fn cancel_all_active_batches_for_part<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            r#"
            UPDATE t_part_batch
            SET status     = 'CANCELLED',
                version    = version + 1,
                updated_at = now(),
                updated_by = $2
            WHERE part_id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(part_id)
        .bind(current_user_id)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }
}

// ====== 原 `repo/event.rs` 全文搬迁 ======
//
// 1 个 `insert_part_event`：service 层在事务内统一插入状态翻转 / 批次拆分 / 返修
// 等事件；本方法只负责 `INSERT`。

use crate::modules::part::model::NewPartEvent;

impl PartRepo {
    /// 插入 `t_part_event` 事件日志。
    ///
    /// `id` 由 caller 用 `SnowflakeIdGenerator::next_id()` 预生成；
    /// `created_at` 走 DB 默认 `now()`。
    pub async fn insert_part_event<'e, E: PgExecutor<'e>>(
        executor: E,
        e: NewPartEvent<'_>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_part_event (
                id, part_id, event_type, from_status, to_status,
                batch_id, quantity, drawing_code, badge_code, note,
                created_at, created_by
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now(), $11
            )
            "#,
            e.id,
            e.part_id,
            e.event_type,
            e.from_status,
            e.to_status,
            e.batch_id,
            e.quantity,
            e.drawing_code,
            e.badge_code,
            e.note,
            e.created_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }
}