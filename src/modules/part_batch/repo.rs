//! part_batch 域数据访问
//!
//! 对应 Python myERP/repository/part_batch_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P1（送货分组）只暴露 delivery_note / delivery_group 后续会用到的点：
//! - `get_by_id`
//! - `list_by_delivery_note` —— 送货单行项目列表
//! - `list_with_part_by_delivery_note` —— 同上，JOIN t_part 一次拿齐（防 N+1）
//! - `list_by_part_ids` —— 多工单批次批查（按 part_id in）
//! - `update` —— 扫码入单 / 手工 add-parts 写 delivery_note_id + status；带乐观锁
//!
//! Phase P3 还需要 `split_batch`（手工部分量），到 part_batch 域实施阶段再加。
//!
//! worker-pool 域新增：
//! - `count_held_by_worker` —— worker 当前持有批次数（state 端点 + max_held_batches 校验）
//! - `list_held_by_worker` —— worker 当前持有批次详情（state 端点 DTO）

use sqlx::PgExecutor;

use super::model::{RecentBatchRow, TPartBatch};
use crate::modules::part::model::TPart;

pub struct PartBatchRepo;

impl PartBatchRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, next_process_id, placed_at,
                   delivery_note_id, parent_batch_id, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_batch
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 送货单的全部未删批次（Phase P2 列出 / Phase P3 扫码入单后回查）。
    /// 与 Python `list_by_delivery_note` 行为一致；本方法不 JOIN t_part，
    /// caller 需要展示字段时另调 `list_with_part_by_delivery_note`。
    pub async fn list_by_delivery_note<'e, E: PgExecutor<'e>>(
        executor: E,
        note_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, next_process_id, placed_at,
                   delivery_note_id, parent_batch_id, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_batch
            WHERE delivery_note_id = $1
              AND deleted_at IS NULL
            ORDER BY id ASC
            "#,
            note_id,
        )
        .fetch_all(executor)
        .await
    }

    /// 送货单的全部未删批次 + 对应工单展示字段（Phase P2 列表输出需要：`name` /
    /// `drawing_no` 等不存于 `t_part_batch`，要 JOIN `t_part`）。
    pub async fn list_with_part_by_delivery_note<'e, E: PgExecutor<'e>>(
        executor: E,
        note_id: i64,
    ) -> Result<Vec<(TPartBatch, TPart)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"
            SELECT
                pb.id            AS "pb_id!",
                pb.part_id       AS "pb_part_id!",
                pb.batch_no      AS "pb_batch_no!",
                pb.quantity      AS "pb_quantity!",
                pb.status        AS "pb_status!",
                pb.location      AS "pb_location?",
                pb.current_holder_id AS "pb_current_holder_id?",
                pb.next_process_id AS "pb_next_process_id?",
                pb.placed_at     AS "pb_placed_at?",
                pb.delivery_note_id AS "pb_delivery_note_id?",
                pb.parent_batch_id AS "pb_parent_batch_id?",
                pb.has_been_repaired AS "pb_has_been_repaired!",
                pb.version       AS "pb_version!",
                pb.created_at    AS "pb_created_at!",
                pb.created_by    AS "pb_created_by?",
                pb.updated_at    AS "pb_updated_at!",
                pb.updated_by    AS "pb_updated_by?",
                pb.deleted_at    AS "pb_deleted_at?",
                p.id             AS "p_id!",
                p.serial_no      AS "p_serial_no?",
                p.name           AS "p_name!",
                p.drawing_no     AS "p_drawing_no!",
                p.customer_id    AS "p_customer_id!",
                p.assembly_id    AS "p_assembly_id?",
                p.status         AS "p_status!",
                p.version        AS "p_version!",
                p.created_at     AS "p_created_at!",
                p.created_by     AS "p_created_by?",
                p.updated_at     AS "p_updated_at!",
                p.updated_by     AS "p_updated_by?",
                p.deleted_at     AS "p_deleted_at?",
                p.delivery_note_id AS "p_delivery_note_id?",
                p.current_holder_id AS "p_current_holder_id?",
                p.next_process_id   AS "p_next_process_id?",
                p.applicant_name  AS "p_applicant_name!",
                p.quantity        AS "p_quantity!",
                p.request_date    AS "p_request_date!",
                p.planned_delivery_date AS "p_planned_delivery_date!",
                p.actual_delivery_date  AS "p_actual_delivery_date?",
                p.location        AS "p_location?",
                p.is_urgent       AS "p_is_urgent!",
                p.placed_at       AS "p_placed_at?",
                p.order_no        AS "p_order_no?",
                p.system_delivery_date AS "p_system_delivery_date?",
                p.note            AS "p_note?",
                p.has_been_repaired AS "p_has_been_repaired!"
            FROM t_part_batch pb
            JOIN t_part p ON p.id = pb.part_id
            WHERE pb.delivery_note_id = $1
              AND pb.deleted_at IS NULL
              AND p.deleted_at IS NULL
            ORDER BY pb.id ASC
            "#,
            note_id,
        )
        .fetch_all(executor)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    TPartBatch {
                        id: r.pb_id,
                        part_id: r.pb_part_id,
                        batch_no: r.pb_batch_no,
                        quantity: r.pb_quantity,
                        status: r.pb_status,
                        location: r.pb_location,
                        current_holder_id: r.pb_current_holder_id,
                        next_process_id: r.pb_next_process_id,
                        placed_at: r.pb_placed_at,
                        delivery_note_id: r.pb_delivery_note_id,
                        parent_batch_id: r.pb_parent_batch_id,
                        has_been_repaired: r.pb_has_been_repaired,
                        version: r.pb_version,
                        created_at: r.pb_created_at,
                        created_by: r.pb_created_by,
                        updated_at: r.pb_updated_at,
                        updated_by: r.pb_updated_by,
                        deleted_at: r.pb_deleted_at,
                    },
                    TPart {
                        id: r.p_id,
                        serial_no: r.p_serial_no,
                        name: r.p_name,
                        drawing_no: r.p_drawing_no,
                        applicant_name: r.p_applicant_name,
                        quantity: r.p_quantity,
                        request_date: r.p_request_date,
                        planned_delivery_date: r.p_planned_delivery_date,
                        actual_delivery_date: r.p_actual_delivery_date,
                        customer_id: r.p_customer_id,
                        assembly_id: r.p_assembly_id,
                        status: r.p_status,
                        location: r.p_location,
                        is_urgent: r.p_is_urgent,
                        current_holder_id: r.p_current_holder_id,
                        placed_at: r.p_placed_at,
                        next_process_id: r.p_next_process_id,
                        order_no: r.p_order_no,
                        system_delivery_date: r.p_system_delivery_date,
                        note: r.p_note,
                        has_been_repaired: r.p_has_been_repaired,
                        version: r.p_version,
                        created_at: r.p_created_at,
                        created_by: r.p_created_by,
                        updated_at: r.p_updated_at,
                        updated_by: r.p_updated_by,
                        deleted_at: r.p_deleted_at,
                        delivery_note_id: r.p_delivery_note_id,
                    },
                )
            })
            .collect())
    }

    /// 多送货单的「批次 + 工单展示字段」批查（PR3 batch-detail 专用）。
    ///
    /// 与 `list_with_part_by_delivery_note` 同投影；改用 `WHERE pb.delivery_note_id = ANY($1)`，
    /// 由 caller 按 `b.delivery_note_id` 分桶后组装 N 个 `DeliveryNoteDetailOut`。
    /// 空输入短路（避免 `ANY($1::bigint[])` 抛 sqlx 类型推断错）。
    pub async fn list_with_part_by_delivery_note_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        note_ids: &[i64],
    ) -> Result<Vec<(TPartBatch, TPart)>, sqlx::Error> {
        if note_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            r#"
            SELECT
                pb.id            AS "pb_id!",
                pb.part_id       AS "pb_part_id!",
                pb.batch_no      AS "pb_batch_no!",
                pb.quantity      AS "pb_quantity!",
                pb.status        AS "pb_status!",
                pb.location      AS "pb_location?",
                pb.current_holder_id AS "pb_current_holder_id?",
                pb.next_process_id AS "pb_next_process_id?",
                pb.placed_at     AS "pb_placed_at?",
                pb.delivery_note_id AS "pb_delivery_note_id?",
                pb.parent_batch_id AS "pb_parent_batch_id?",
                pb.has_been_repaired AS "pb_has_been_repaired!",
                pb.version       AS "pb_version!",
                pb.created_at    AS "pb_created_at!",
                pb.created_by    AS "pb_created_by?",
                pb.updated_at    AS "pb_updated_at!",
                pb.updated_by    AS "pb_updated_by?",
                pb.deleted_at    AS "pb_deleted_at?",
                p.id             AS "p_id!",
                p.serial_no      AS "p_serial_no?",
                p.name           AS "p_name!",
                p.drawing_no     AS "p_drawing_no!",
                p.customer_id    AS "p_customer_id!",
                p.assembly_id    AS "p_assembly_id?",
                p.status         AS "p_status!",
                p.version        AS "p_version!",
                p.created_at     AS "p_created_at!",
                p.created_by     AS "p_created_by?",
                p.updated_at     AS "p_updated_at!",
                p.updated_by     AS "p_updated_by?",
                p.deleted_at     AS "p_deleted_at?",
                p.delivery_note_id AS "p_delivery_note_id?",
                p.current_holder_id AS "p_current_holder_id?",
                p.next_process_id   AS "p_next_process_id?",
                p.applicant_name  AS "p_applicant_name!",
                p.quantity        AS "p_quantity!",
                p.request_date    AS "p_request_date!",
                p.planned_delivery_date AS "p_planned_delivery_date!",
                p.actual_delivery_date  AS "p_actual_delivery_date?",
                p.location        AS "p_location?",
                p.is_urgent       AS "p_is_urgent!",
                p.placed_at       AS "p_placed_at?",
                p.order_no        AS "p_order_no?",
                p.system_delivery_date AS "p_system_delivery_date?",
                p.note            AS "p_note?",
                p.has_been_repaired AS "p_has_been_repaired!"
            FROM t_part_batch pb
            JOIN t_part p ON p.id = pb.part_id
            WHERE pb.delivery_note_id = ANY($1)
              AND pb.deleted_at IS NULL
              AND p.deleted_at IS NULL
            ORDER BY pb.id ASC
            "#,
            note_ids,
        )
        .fetch_all(executor)
        .await?;

        Ok(rows.into_iter().map(|r| {
            (
                TPartBatch {
                    id: r.pb_id,
                    part_id: r.pb_part_id,
                    batch_no: r.pb_batch_no,
                    quantity: r.pb_quantity,
                    status: r.pb_status,
                    location: r.pb_location,
                    current_holder_id: r.pb_current_holder_id,
                    next_process_id: r.pb_next_process_id,
                    placed_at: r.pb_placed_at,
                    delivery_note_id: r.pb_delivery_note_id,
                    parent_batch_id: r.pb_parent_batch_id,
                    has_been_repaired: r.pb_has_been_repaired,
                    version: r.pb_version,
                    created_at: r.pb_created_at,
                    created_by: r.pb_created_by,
                    updated_at: r.pb_updated_at,
                    updated_by: r.pb_updated_by,
                    deleted_at: r.pb_deleted_at,
                },
                TPart {
                    id: r.p_id,
                    serial_no: r.p_serial_no,
                    name: r.p_name,
                    drawing_no: r.p_drawing_no,
                    applicant_name: r.p_applicant_name,
                    quantity: r.p_quantity,
                    request_date: r.p_request_date,
                    planned_delivery_date: r.p_planned_delivery_date,
                    actual_delivery_date: r.p_actual_delivery_date,
                    customer_id: r.p_customer_id,
                    assembly_id: r.p_assembly_id,
                    status: r.p_status,
                    location: r.p_location,
                    is_urgent: r.p_is_urgent,
                    current_holder_id: r.p_current_holder_id,
                    placed_at: r.p_placed_at,
                    next_process_id: r.p_next_process_id,
                    order_no: r.p_order_no,
                    system_delivery_date: r.p_system_delivery_date,
                    note: r.p_note,
                    has_been_repaired: r.p_has_been_repaired,
                    version: r.p_version,
                    created_at: r.p_created_at,
                    created_by: r.p_created_by,
                    updated_at: r.p_updated_at,
                    updated_by: r.p_updated_by,
                    deleted_at: r.p_deleted_at,
                    delivery_note_id: r.p_delivery_note_id,
                },
            )
        }).collect())
    }

    /// 多工单的未删批次批查（Phase P3 装配件整套入单时按子件 part_ids 一次拿齐）。
    pub async fn list_by_part_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        part_ids: &[i64],
        include_deleted: bool,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        if part_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, next_process_id, placed_at,
                   delivery_note_id, parent_batch_id, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_batch
            WHERE part_id = ANY($1)
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY part_id ASC, batch_no ASC
            "#,
            part_ids,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// version-checked 部分更新：仅 `delivery_note_id` + `status` 两个字段。
    /// 用于 Phase P3 扫码入单（写 delivery_note_id）和 Phase P2 手工 add_parts
    /// 同步推进 status。caller 必须已在事务内先读 version。
    /// 返回影响行数（0 行 → 由 service 转 `VERSION_CONFLICT` 409）。
    #[allow(clippy::too_many_arguments)]
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        version: i32,
        delivery_note_id: Option<i64>,
        status: Option<&str>,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET delivery_note_id = COALESCE($3::bigint, delivery_note_id),
                status           = COALESCE($4::varchar, status),
                version          = version + 1,
                updated_at       = $5,
                updated_by       = $6
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            batch_id,
            version,
            delivery_note_id,
            status,
            when,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }

    /// version-checked 「仅写 delivery_note_id」更新（attach_to_note 用）。
    /// `attach_to_note`：把一个批次挂到指定送货单（不改 status / 其它列）；
    /// 0 行 → version 冲突由 service 转 `VERSION_CONFLICT` 409。
    pub async fn attach_to_note<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        note_id: i64,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET delivery_note_id = $3,
                version          = version + 1,
                updated_at       = $4,
                updated_by       = $5
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            note_id,
            when,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }

    /// 拆分批次：在 `qty` < `batch.quantity` 时调用，构造一条新批次（继承
    /// 状态/位置/holder/next_process；**不继承** delivery_note_id 与
    /// parent_batch_id），并把源批次 quantity 减 `qty`。整组写在一个 tx 内。
    ///
    /// 返回新批次雪花 id（caller 拿到后做后续 attach_to_note）。`batch_no` 用
    /// 「当前 part_id 下 max(batch_no) + 1」生成。
    ///
    /// 镜像 Python `service/_batch_ops::split_batch`。
    ///
    /// 注：本函数需在同一事务内连发三条 SQL（max + insert + update），而
    /// `impl PgExecutor<'_>` 不能 move 多次，因此显式收 `&mut PgConnection`。
    #[allow(clippy::too_many_arguments)]
    pub async fn split_batch(
        conn: &mut sqlx::PgConnection,
        new_batch_id: i64,
        source_batch_id: i64,
        source_version: i32,
        part_id: i64,
        qty: i32,
        status: &str,
        location: Option<&str>,
        current_holder_id: Option<i64>,
        next_process_id: Option<i64>,
        placed_at: Option<chrono::NaiveDateTime>,
        when: chrono::NaiveDateTime,
        created_by: Option<i64>,
        updated_by: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        // 1. 新 batch_no：同 part_id 下 max + 1（与 uq_t_part_batch_part_no 对齐）。
        let next_batch_no: i32 = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(MAX(batch_no), 0) + 1 AS "next!"
            FROM t_part_batch
            WHERE part_id = $1 AND deleted_at IS NULL
            "#,
            part_id,
        )
        .fetch_one(&mut *conn)
        .await?;

        // 2. 插入新批次（quantity = qty，不继承 delivery_note_id，写 parent_batch_id）。
        sqlx::query!(
            r#"
            INSERT INTO t_part_batch
                (id, part_id, batch_no, quantity, status, location,
                 current_holder_id, next_process_id, placed_at,
                 delivery_note_id, parent_batch_id, has_been_repaired,
                 version, created_at, created_by, updated_at, updated_by)
            VALUES ($1, $2, $3, $4, $5, $6,
                    $7, $8, $9,
                    NULL, $10, FALSE,
                    0, $11, $12, $11, $13)
            "#,
            new_batch_id,
            part_id,
            next_batch_no,
            qty,
            status,
            location,
            current_holder_id,
            next_process_id,
            placed_at,
            source_batch_id,
            when,
            created_by,
            updated_by,
        )
        .execute(&mut *conn)
        .await?;

        // 3. 源批次 quantity -= qty（带 version 校验；0 行 → conflict）。
        let res = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET quantity    = quantity - $3,
                version     = version + 1,
                updated_at  = $4,
                updated_by  = $5
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            source_batch_id,
            source_version,
            qty,
            when,
            updated_by,
        )
        .execute(&mut *conn)
        .await?;
        if res.rows_affected() == 0 {
            return Err(sqlx::Error::RowNotFound);
        }

        Ok(new_batch_id)
    }

    /// 工单全部活跃批次 + 批次自身状态（rollup 用）。
    /// 不传 `include_deleted`：rollup 只看活跃行。
    pub async fn list_active_by_part_id<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, next_process_id, placed_at,
                   delivery_note_id, parent_batch_id, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_batch
            WHERE part_id = $1 AND deleted_at IS NULL
            ORDER BY batch_no ASC
            "#,
            part_id,
        )
        .fetch_all(executor)
        .await
    }

    /// 多 part 全部活跃批次批查（pickup rollup 用）。
    pub async fn list_active_by_part_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        part_ids: &[i64],
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        if part_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, next_process_id, placed_at,
                   delivery_note_id, parent_batch_id, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_batch
            WHERE part_id = ANY($1) AND deleted_at IS NULL
            ORDER BY part_id ASC, batch_no ASC
            "#,
            part_ids,
        )
        .fetch_all(executor)
        .await
    }

    /// 候选入单池（list_candidate_parts 用）：状态 ∈ {INSPECTION, READY_TO_SHIP}，
    /// 非软删，工单非软删，客户 ∈ customer_ids。
    /// 与 Python `PartBatchRepository.list_batches_with_part` 对齐。
    pub async fn list_batches_with_part_in_customers<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        customer_ids: &[i64],
        limit: i64,
    ) -> Result<Vec<(TPartBatch, TPart)>, sqlx::Error> {
        if customer_ids.is_empty() || statuses.is_empty() {
            return Ok(Vec::new());
        }
        // 用 sqlx::query_as + FromRow 风格：先把单行 decode 成 (TPartBatch, TPart)。
        // SQL 静态（不带 format!），用 query_as! 与任意 ANY 绑定需要 `text[]` / `bigint[]`。
        // 这里 owner 表用 status ANY($1) + customer_id ANY($2)，传入 `&[&str]` / `&[i64]`
        // 由 sqlx 编码为 text[] / bigint[]。
        let sql = r#"
            SELECT
                pb.id, pb.part_id, pb.batch_no, pb.quantity, pb.status, pb.location,
                pb.current_holder_id, pb.next_process_id, pb.placed_at,
                pb.delivery_note_id, pb.parent_batch_id, pb.has_been_repaired,
                pb.version, pb.created_at, pb.created_by, pb.updated_at, pb.updated_by, pb.deleted_at,
                p.id AS "p_id", p.serial_no AS "p_serial_no", p.name AS "p_name",
                p.drawing_no AS "p_drawing_no", p.customer_id AS "p_customer_id",
                p.assembly_id AS "p_assembly_id", p.status AS "p_status",
                p.version AS "p_version", p.created_at AS "p_created_at",
                p.created_by AS "p_created_by", p.updated_at AS "p_updated_at",
                p.updated_by AS "p_updated_by", p.deleted_at AS "p_deleted_at",
                p.delivery_note_id AS "p_delivery_note_id",
                p.applicant_name AS "p_applicant_name",
                p.quantity AS "p_quantity",
                p.request_date AS "p_request_date",
                p.planned_delivery_date AS "p_planned_delivery_date",
                p.actual_delivery_date AS "p_actual_delivery_date",
                p.location AS "p_location",
                p.is_urgent AS "p_is_urgent",
                p.current_holder_id AS "p_current_holder_id",
                p.placed_at AS "p_placed_at",
                p.next_process_id AS "p_next_process_id",
                p.order_no AS "p_order_no",
                p.system_delivery_date AS "p_system_delivery_date",
                p.note AS "p_note",
                p.has_been_repaired AS "p_has_been_repaired"
            FROM t_part_batch pb
            JOIN t_part p ON p.id = pb.part_id
            WHERE pb.deleted_at IS NULL
              AND p.deleted_at  IS NULL
              AND pb.status = ANY($1)
              AND p.customer_id = ANY($2)
            ORDER BY p.serial_no ASC NULLS LAST, pb.id ASC
            LIMIT $3
        "#;

        let rows: Vec<sqlx::postgres::PgRow> = sqlx::query(sql)
            .bind(statuses)
            .bind(customer_ids)
            .bind(limit)
            .fetch_all(executor)
            .await?;

        use sqlx::Row;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let pb = TPartBatch {
                id: r.try_get("id")?,
                part_id: r.try_get("part_id")?,
                batch_no: r.try_get("batch_no")?,
                quantity: r.try_get("quantity")?,
                status: r.try_get("status")?,
                location: r.try_get("location")?,
                current_holder_id: r.try_get("current_holder_id")?,
                next_process_id: r.try_get("next_process_id")?,
                placed_at: r.try_get("placed_at")?,
                delivery_note_id: r.try_get("delivery_note_id")?,
                parent_batch_id: r.try_get("parent_batch_id")?,
                has_been_repaired: r.try_get("has_been_repaired")?,
                version: r.try_get("version")?,
                created_at: r.try_get("created_at")?,
                created_by: r.try_get("created_by")?,
                updated_at: r.try_get("updated_at")?,
                updated_by: r.try_get("updated_by")?,
                deleted_at: r.try_get("deleted_at")?,
            };
            let p = TPart {
                id: r.try_get("p_id")?,
                serial_no: r.try_get("p_serial_no")?,
                name: r.try_get("p_name")?,
                drawing_no: r.try_get("p_drawing_no")?,
                applicant_name: r.try_get("p_applicant_name")?,
                quantity: r.try_get("p_quantity")?,
                request_date: r.try_get("p_request_date")?,
                planned_delivery_date: r.try_get("p_planned_delivery_date")?,
                actual_delivery_date: r.try_get("p_actual_delivery_date")?,
                customer_id: r.try_get("p_customer_id")?,
                assembly_id: r.try_get("p_assembly_id")?,
                status: r.try_get("p_status")?,
                location: r.try_get("p_location")?,
                is_urgent: r.try_get("p_is_urgent")?,
                current_holder_id: r.try_get("p_current_holder_id")?,
                placed_at: r.try_get("p_placed_at")?,
                next_process_id: r.try_get("p_next_process_id")?,
                order_no: r.try_get("p_order_no")?,
                system_delivery_date: r.try_get("p_system_delivery_date")?,
                note: r.try_get("p_note")?,
                has_been_repaired: r.try_get("p_has_been_repaired")?,
                version: r.try_get("p_version")?,
                created_at: r.try_get("p_created_at")?,
                created_by: r.try_get("p_created_by")?,
                updated_at: r.try_get("p_updated_at")?,
                updated_by: r.try_get("p_updated_by")?,
                deleted_at: r.try_get("p_deleted_at")?,
                delivery_note_id: r.try_get("p_delivery_note_id")?,
            };
            out.push((pb, p));
        }
        Ok(out)
    }

    /// 草稿卡片「最近加入批次」展示数据。
    ///
    /// 2026-08-22 新增：配合 `ScanDeliveryNoteSummaryDto::recent_items`。
    /// JOIN `t_part` 拿 `serial_no` / `drawing_no` / `name` / `order_no` 展示列，
    /// 按 batch id DESC 取最近 `limit` 条（业务约定 limit=8）。
    ///
    /// 注：原 plan 草稿 SQL 里 `b.serial_no` 是错的 —— `t_part_batch` 没有
    /// `serial_no` 列，那列在 `t_part` 上。这里修正为 `p.serial_no`。
    pub async fn list_recent_by_note<'e, E: PgExecutor<'e>>(
        executor: E,
        note_id: i64,
        limit: i64,
    ) -> Result<Vec<RecentBatchRow>, sqlx::Error> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            r#"
            SELECT
                b.id           AS "b_id!",
                b.part_id      AS "b_part_id!",
                p.serial_no    AS "p_serial_no?",
                p.drawing_no   AS "p_drawing_no!",
                p.name         AS "p_name!",
                p.order_no     AS "p_order_no?"
            FROM t_part_batch b
            JOIN t_part p ON p.id = b.part_id
            WHERE b.delivery_note_id = $1
              AND b.deleted_at IS NULL
              AND p.deleted_at IS NULL
            ORDER BY b.id DESC
            LIMIT $2
            "#,
            note_id,
            limit,
        )
        .fetch_all(executor)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| RecentBatchRow {
                batch_id: r.b_id,
                part_id: r.b_part_id,
                serial_no: r.p_serial_no,
                drawing_no: r.p_drawing_no,
                name: r.p_name,
                order_no: r.p_order_no,
            })
            .collect())
    }

    /// 工人当前持有批次数（worker-pool used/max 校验）。
    /// 复用 `ix_t_part_batch_holder_location`（Task 1 已建）覆盖
    /// `(current_holder_id, location)` 谓词。
    pub async fn count_held_by_worker<'e, E: PgExecutor<'e>>(
        executor: E,
        worker_id: i64,
    ) -> Result<i64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch
            WHERE current_holder_id = $1
              AND location = 'WORKER'
              AND deleted_at IS NULL
            "#,
            worker_id,
        )
        .fetch_one(executor)
        .await?;
        Ok(n)
    }

    /// 工人当前持有批次列表（worker-pool state 端点用）。
    /// 同样命中 `ix_t_part_batch_holder_location`。
    pub async fn list_held_by_worker<'e, E: PgExecutor<'e>>(
        executor: E,
        worker_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, next_process_id, placed_at,
                   delivery_note_id, parent_batch_id, has_been_repaired,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_batch
            WHERE current_holder_id = $1
              AND location = 'WORKER'
              AND deleted_at IS NULL
            ORDER BY id ASC
            "#,
            worker_id,
        )
        .fetch_all(executor)
        .await
    }
}