//! `t_part_batch` 批次查询 + 状态机 UPDATE
//!
//! 既有 14 个方法搬迁：
//! - 3 个 `find_*`（inprocess / scan_target / inspection_for_fail）
//! - 1 个 `count_other_inprocess_batches`
//! - 6 个 `mark_*_passed_inspection` / `mark_*_inspected` / `mark_*_failed_inspection`
//!   （part / batch 各一对共 6 个）
//! - 1 个 `split_batch_for_partial_pass`
//!
//! Phase PR-CRUD 新增 8 个 `mark_*_lifecycle`（delivered / completed /
//! cancelled / repairing 各 part + batch 一对），与 6 个 `mark_*_inspection`
//! 同形但状态守卫不同。

use sqlx::{PgConnection, PgExecutor};

use crate::modules::part_batch::model::TPartBatch;

use super::PartRepo;

impl PartRepo {
    /// 定位 part 的 INSPECTION 状态批次。
    ///
    /// - `expected_batch_id = None`：先 COUNT 校验唯一性（≥2 → 歧义 `RowNotFound`），
    ///   == 0 → `Ok(None)`，== 1 → 取 id 最小者。
    /// - `expected_batch_id = Some(bid)`：按 id 校验 ownership。
    ///
    /// 签名收 `&mut PgConnection`：方法在 `None` 分支需在同一事务内连发两条 SQL。
    pub async fn find_inprocess_batch_for_part(
        conn: &mut PgConnection,
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
            .fetch_optional(&mut *conn)
            .await,
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
                    1 => sqlx::query_as!(
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
                    .fetch_optional(&mut *conn)
                    .await,
                    _ => Err(sqlx::Error::RowNotFound),
                }
            }
        }
    }

    /// 定位 scan-inspect 的目标批次（白名单 `{PENDING, PROGRAMMING, IN_PROCESS}`）。
    pub async fn find_scan_target_batch(
        conn: &mut PgConnection,
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
                WHERE id = $1 AND part_id = $2
                  AND status IN ('PENDING', 'PROGRAMMING', 'IN_PROCESS')
                  AND deleted_at IS NULL
                "#,
                bid,
                part_id,
            )
            .fetch_optional(&mut *conn)
            .await,
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
                    1 => sqlx::query_as!(
                        TPartBatch,
                        r#"
                        SELECT id, part_id, batch_no, quantity, status, location,
                               current_holder_id, next_process_id, placed_at,
                               delivery_note_id, parent_batch_id, has_been_repaired,
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
                    .await,
                    _ => Err(sqlx::Error::RowNotFound),
                }
            }
        }
    }

    /// 定位 fail-inspection 的目标 INSPECTION 批次。
    pub async fn find_inspection_batch_for_fail(
        conn: &mut PgConnection,
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
            .fetch_optional(&mut *conn)
            .await,
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
                    1 => sqlx::query_as!(
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
                    .fetch_optional(&mut *conn)
                    .await,
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

    /// 统计 part 仍处于 INSPECTION 状态的非软删批次数量。
    pub async fn count_other_inprocess_batches<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<i64, sqlx::Error> {
        let count: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch
            WHERE part_id = $1 AND status = 'INSPECTION' AND deleted_at IS NULL
            "#,
            part_id,
        )
        .fetch_one(executor)
        .await?;
        Ok(count)
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

    /// pass_inspection 后同步工单状态（OCC UPDATE `t_part.status`）。
    ///
    /// **必须与 `mark_batch_passed_inspection` 在同一事务内调用**。
    pub async fn mark_part_passed_inspection<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part
            SET status     = 'READY_TO_SHIP',
                version    = version + 1,
                updated_at = now(),
                updated_by = $3
            WHERE id = $1 AND version = $2 AND status = 'INSPECTION'
              AND deleted_at IS NULL
            "#,
            part_id,
            expected_version,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// scan-inspect 第一步：工单搬到品检架（OCC UPDATE t_part）。
    pub async fn mark_part_inspected<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part
            SET status            = 'INSPECTION',
                location          = 'INSPECTION_SHELF',
                current_holder_id = $3,
                version           = version + 1,
                updated_at        = now(),
                updated_by        = $4
            WHERE id = $1 AND version = $2
              AND status IN ('PENDING', 'PROGRAMMING', 'IN_PROCESS')
              AND deleted_at IS NULL
            "#,
            part_id,
            expected_version,
            shelf_id,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// scan-inspect 第一步：批次状态同步（OCC UPDATE t_part_batch）。
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

    /// fail-inspection：批次打回生产架（OCC UPDATE t_part_batch）。
    pub async fn mark_batch_failed_inspection<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        next_process_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET status            = 'IN_PROCESS',
                location          = 'PRODUCTION_SHELF',
                current_holder_id = $3,
                next_process_id   = $4,
                version           = version + 1,
                updated_at        = now(),
                updated_by        = $5
            WHERE id = $1 AND version = $2 AND status = 'INSPECTION'
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            shelf_id,
            next_process_id,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// fail-inspection：工单状态同步（OCC UPDATE t_part）。
    pub async fn mark_part_failed_inspection<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        shelf_id: i64,
        next_process_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part
            SET status            = 'IN_PROCESS',
                location          = 'PRODUCTION_SHELF',
                current_holder_id = $3,
                next_process_id   = $4,
                version           = version + 1,
                updated_at        = now(),
                updated_by        = $5
            WHERE id = $1 AND version = $2 AND status = 'INSPECTION'
              AND deleted_at IS NULL
            "#,
            part_id,
            expected_version,
            shelf_id,
            next_process_id,
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
    pub async fn find_inprocess_batch_by_id_and_holder(
        conn: &mut PgConnection,
        batch_id: i64,
        holder_id: i64,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, next_process_id, placed_at,
                   delivery_note_id, parent_batch_id, has_been_repaired,
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
    ///   `next_process_id = $4`，`version += 1`。
    ///
    /// `current_user_id` 写入 `updated_by`（nullable 与既有路径一致）。
    pub async fn mark_batch_returned<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        next_process_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET current_holder_id = $3,
                location          = 'PRODUCTION_SHELF',
                next_process_id   = $4,
                version           = version + 1,
                updated_at        = now(),
                updated_by        = $5
            WHERE id = $1 AND version = $2
              AND status = 'IN_PROCESS' AND location = 'WORKER'
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            shelf_id,
            next_process_id,
            current_user_id,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// worker-pool admin_remove / worker-scan RETURNED 用：工单 holder worker → shelf（OCC）。
    ///
    /// 与 `mark_batch_returned` 同事务调用。
    /// 0 行 → 40901 VERSION_CONFLICT / 工单已软删 —— 由 service 层映射。
    /// 成功 → `current_holder_id = shelf_id`，`location = 'PRODUCTION_SHELF'`，
    ///   `next_process_id = $4`，`version += 1`。
    pub async fn mark_part_returned<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        shelf_id: i64,
        next_process_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part
            SET current_holder_id = $3,
                location          = 'PRODUCTION_SHELF',
                next_process_id   = $4,
                version           = version + 1,
                updated_at        = now(),
                updated_by        = $5
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            part_id,
            expected_version,
            shelf_id,
            next_process_id,
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
    pub async fn find_worker_held_batch_for_part(
        conn: &mut PgConnection,
        part_id: i64,
        worker_id: i64,
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
                WHERE id = $1 AND part_id = $2 AND current_holder_id = $3
                  AND status = 'IN_PROCESS' AND location = 'WORKER'
                  AND deleted_at IS NULL
                "#,
                bid,
                part_id,
                worker_id,
            )
            .fetch_optional(&mut *conn)
            .await,
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
                    1 => sqlx::query_as!(
                        TPartBatch,
                        r#"
                        SELECT id, part_id, batch_no, quantity, status, location,
                               current_holder_id, next_process_id, placed_at,
                               delivery_note_id, parent_batch_id, has_been_repaired,
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
                    .await,
                    // ≥2 个 IN_PROCESS+WORKER 批次：歧义。Service 层负责把
                    // `sqlx::Error::RowNotFound` 翻译为 `AppError::Biz` /
                    // `20114 / BIZ_PART_BATCH_NOT_HELD_BY_WORKER`。
                    _ => Err(sqlx::Error::RowNotFound),
                }
            }
        }
    }

    /// 部分通过：拆出 INSPECTION 批次。
    ///
    /// 原子化三步（共享事务）：
    /// 1. 算同 part_id 下下一个 `batch_no`（max + 1）
    /// 2. INSERT 新 INSPECTION 批次（quantity = split_quantity）
    /// 3. UPDATE 源批次 `quantity -= split_quantity`（OCC + 数量守卫）
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
        // 1. 算 next batch_no
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

        // 2. INSERT 新 INSPECTION 批次（quantity = split_quantity）
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

        // 3. UPDATE 源批次 quantity -= split_quantity
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
            return Err(sqlx::Error::RowNotFound);
        }

        Ok(new_batch_id)
    }

    // ===== Phase PR-CRUD 新增：8 个 lifecycle mark_* =====

    /// 工单 READY_TO_SHIP → DELIVERED（OCC UPDATE t_part）。
    pub async fn mark_part_delivered<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part SET status='DELIVERED', version=version+1,
                updated_at=now(), updated_by=$3
               WHERE id=$1 AND version=$2 AND status='READY_TO_SHIP' AND deleted_at IS NULL"#,
            part_id, expected_version, current_user_id,
        ).execute(executor).await?;
        Ok(r.rows_affected())
    }

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
            batch_id, expected_version, current_user_id,
        ).execute(executor).await?;
        Ok(r.rows_affected())
    }

    /// 工单 DELIVERED → COMPLETED：清空 `serial_no`（序列号已被送货单占用）。
    pub async fn mark_part_completed<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part SET status='COMPLETED', version=version+1,
                updated_at=now(), updated_by=$3, serial_no=NULL
               WHERE id=$1 AND version=$2 AND status='DELIVERED' AND deleted_at IS NULL"#,
            part_id, expected_version, current_user_id,
        ).execute(executor).await?;
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
            batch_id, expected_version, current_user_id,
        ).execute(executor).await?;
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
            part_id, expected_version, current_user_id,
        ).execute(executor).await?;
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
            batch_id, expected_version, current_user_id,
        ).execute(executor).await?;
        Ok(r.rows_affected())
    }

    /// 工单 IN_PROCESS → REPAIRING：同时置 `has_been_repaired=true`。
    pub async fn mark_part_repairing<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part SET status='REPAIRING', version=version+1,
                updated_at=now(), updated_by=$3, has_been_repaired=true
               WHERE id=$1 AND version=$2 AND status='IN_PROCESS' AND deleted_at IS NULL"#,
            part_id, expected_version, current_user_id,
        ).execute(executor).await?;
        Ok(r.rows_affected())
    }

    /// 批次 IN_PROCESS → REPAIRING：同时置 `has_been_repaired=true`。
    pub async fn mark_batch_repairing<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"UPDATE t_part_batch SET status='REPAIRING', version=version+1,
                updated_at=now(), updated_by=$3, has_been_repaired=true
               WHERE id=$1 AND version=$2 AND status='IN_PROCESS' AND deleted_at IS NULL"#,
            batch_id, expected_version, current_user_id,
        ).execute(executor).await?;
        Ok(r.rows_affected())
    }
}
