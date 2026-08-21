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

use sqlx::PgExecutor;

use super::model::TPartBatch;
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
                p.delivery_note_id AS "p_delivery_note_id?"
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
                        customer_id: r.p_customer_id,
                        assembly_id: r.p_assembly_id,
                        status: r.p_status,
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
}