//! part 域数据访问
//!
//! 对应 Python myERP/repository/part_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P1（送货分组）只暴露 delivery_note + delivery_group 后续会用到的只读点：
//! - `get_by_id` / `list_by_ids`
//! - `get_by_serial` —— 扫码入单解析（Phase P3）需要 exact match
//! - `list_children` —— 装配件子件列表（扫码整套入单需要，Phase P3）
//!
//! Phase F（pass_inspection 批量送检）新增：
//! - `get_part_inspected` —— pass_inspection 专用最小投影（含 status/version/quantity 等）
//! - `find_inprocess_batch_for_part` —— 定位 INSPECTION 批次（支持 owner 校验）
//! - `mark_batch_passed_inspection` —— 批量通过（status INSPECTION → READY_TO_SHIP）
//! - `split_batch_for_partial_pass` —— 部分通过：拆 INSPECTION 批次
//! - `insert_part_event` —— 写 `t_part_event` 事件日志
//!
//! 关于 `split_batch_for_partial_pass` 的签名：方法需在同一事务内连发多条 SQL，
//! 而 `impl PgExecutor<'_>` 不能 move 多次，按既有约定收 `&mut PgConnection`
//! （与 `PartBatchRepo::split_batch` 同形）。

use sqlx::{PgConnection, PgExecutor};

use super::model::{NewPartEvent, TPart, TPartInspected};
use crate::modules::part_batch::model::TPartBatch;

pub struct PartRepo;

impl PartRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, customer_id, assembly_id, status,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   delivery_note_id
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
            SELECT id, serial_no, name, drawing_no, customer_id, assembly_id, status,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   delivery_note_id
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
            SELECT id, serial_no, name, drawing_no, customer_id, assembly_id, status,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   delivery_note_id
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
            SELECT id, serial_no, name, drawing_no, customer_id, assembly_id, status,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   delivery_note_id
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
    /// 仅取本流程 + `PartOut` 响应实际用到的列；其它字段（如 `applicant_name` /
    /// `unit_price` / `total_price` / `request_date` 等）留待 part 域业务实施时
    /// 伴随 `rust_decimal` feature 一起上线（NUMERIC 列需要 sqlx feature）。
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
                   order_no, actual_delivery_date, updated_at, updated_by
            FROM t_part
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            part_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 定位 part 的 INSPECTION 状态批次。
    ///
    /// - `expected_batch_id = None`：按 `(part_id, status = 'INSPECTION')` 唯一解析，
    ///   取 id 最小者（与既有按 batch 序号的语义一致）。
    /// - `expected_batch_id = Some(bid)`：按 id 校验 ownership（防止 caller 误传
    ///   其它 part 的 batch id）。
    ///
    /// 两路径均要求 `deleted_at IS NULL`。
    pub async fn find_inprocess_batch_for_part<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        match expected_batch_id {
            Some(bid) => sqlx::query_as!(
                TPartBatch,
                r#"
                SELECT id, part_id, batch_no, quantity, status, location,
                       current_holder_id, next_process_id, placed_at,
                       delivery_note_id, parent_batch_id, has_been_repaired,
                       version, created_at, created_by, updated_at, updated_by,
                       deleted_at
                FROM t_part_batch
                WHERE id = $1 AND part_id = $2 AND status = 'INSPECTION'
                  AND deleted_at IS NULL
                "#,
                bid,
                part_id,
            )
            .fetch_optional(executor)
            .await,
            None => sqlx::query_as!(
                TPartBatch,
                r#"
                SELECT id, part_id, batch_no, quantity, status, location,
                       current_holder_id, next_process_id, placed_at,
                       delivery_note_id, parent_batch_id, has_been_repaired,
                       version, created_at, created_by, updated_at, updated_by,
                       deleted_at
                FROM t_part_batch
                WHERE part_id = $1 AND status = 'INSPECTION' AND deleted_at IS NULL
                ORDER BY id ASC
                LIMIT 1
                "#,
                part_id,
            )
            .fetch_optional(executor)
            .await,
        }
    }

    /// 批量通过（OCC UPDATE）。
    ///
    /// - 0 行 → 版本冲突 / 状态非 INSPECTION / 已软删 —— 由 service 层映射为 `40901`。
    /// - 成功 → `status = 'READY_TO_SHIP'`，`version += 1`，`updated_at = now()`。
    ///
    /// **未触碰 `t_part` 行**：`t_part.status` / `t_part.actual_delivery_date` 由后续 PR
    /// 触发器或 service 显式 UPDATE 同步（不在本 PR 范围内）。当前阶段，
    /// `t_part_batch.status` 是 pass_inspection 流程的 source of truth。
    ///
    /// 注：`t_part_batch` 没有 `actual_delivery_date` 列（migration 006 校验过）；
    /// 该列只存在于 `t_part` / `t_assembly`。
    pub async fn mark_batch_passed_inspection<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET status     = 'READY_TO_SHIP',
                version    = version + 1,
                updated_at = now()
            WHERE id = $1 AND version = $2 AND status = 'INSPECTION'
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// 部分通过：拆出 INSPECTION 批次。
    ///
    /// 原子化三步：
    /// 1. 算同 part_id 下下一个 `batch_no`（max + 1，对齐 `uq_t_part_batch_part_no`）。
    /// 2. INSERT 新 INSPECTION 批次（继承 location/holder/next_process/placed_at/has_been_repaired，
    ///    不继承 delivery_note_id；写 parent_batch_id = src；quantity = split_quantity；version = 0）。
    /// 3. UPDATE 源批次：`quantity -= split_quantity`，`version += 1`，
    ///    `WHERE id=? AND version=? AND quantity > split_quantity`（OCC + 数量校验）。
    ///
    /// 0 行 UPDATE → OCC 冲突或 split_quantity 不合法，由 service 层映射。
    /// 返回新 batch 雪花 id。
    ///
    /// 签名收 `&mut PgConnection`：方法内三步必须共享同一事务（且 sqlx `impl PgExecutor<'_>`
    /// 不能 move 多次），与 `PartBatchRepo::split_batch` 同形。
    ///
    /// `created_by` / `updated_by` 字段记当前操作用户（来自 `CurrentUser.id`）。
    #[allow(clippy::too_many_arguments)]
    pub async fn split_batch_for_partial_pass(
        conn: &mut PgConnection,
        new_batch_id: i64,
        src_batch_id: i64,
        src_version: i32,
        part_id: i64,
        split_quantity: i32,
        current_user_id: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        // 1. 算 next batch_no（对齐 uq_t_part_batch_part_no 唯一约束）。
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

        // 2. INSERT 新 INSPECTION 批次（quantity = split_quantity）。
        sqlx::query!(
            r#"
            INSERT INTO t_part_batch (
                id, part_id, batch_no, quantity, status, location,
                current_holder_id, next_process_id, placed_at,
                delivery_note_id, parent_batch_id, has_been_repaired,
                version, created_at, created_by, updated_at, updated_by
            )
            SELECT $1, part_id, $2, $3, 'INSPECTION', location,
                   current_holder_id, next_process_id, placed_at,
                   NULL, $4, has_been_repaired,
                   0, now(), $5, now(), $5
            FROM t_part_batch
            WHERE id = $6 AND deleted_at IS NULL
            "#,
            new_batch_id,
            next_batch_no,
            split_quantity,
            src_batch_id,
            current_user_id,
            src_batch_id,
        )
        .execute(&mut *conn)
        .await?;

        // 3. UPDATE 源批次 quantity -= split_quantity（OCC + 数量守卫）。
        let res = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET quantity    = quantity - $3,
                version     = version + 1,
                updated_at  = now(),
                updated_by  = $4
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
              AND quantity > $3
            "#,
            src_batch_id,
            src_version,
            split_quantity,
            current_user_id,
        )
        .execute(&mut *conn)
        .await?;
        if res.rows_affected() == 0 {
            // OCC 冲突 / split_quantity 不合法 → 回退给 service 层映射。
            return Err(sqlx::Error::RowNotFound);
        }

        Ok(new_batch_id)
    }

    /// 插入 `t_part_event` 事件日志。
    ///
    /// `id` 由 caller 用 `SnowflakeIdGenerator::next_id()` 预生成；
    /// `created_at` 走 DB 默认 `now()`（与既有 `t_part_event` 写入路径一致）。
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
