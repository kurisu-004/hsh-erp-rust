//! `t_part_batch` 批次查询 + 状态机 UPDATE
//!
//! 既有方法搬迁：
//! - 3 个 `find_*`（inprocess / scan_target / inspection_for_fail）
//! - 6 个 `mark_*_inspection`（passed / failed / inspected，part / batch 各一对）
//! - 1 个 `split_batch_for_partial_pass`
//!
//! Phase PR-CRUD 新增 8 个 `mark_*_lifecycle`（delivered / completed /
//! cancelled / repairing 各 part + batch 一对），与 6 个 `mark_*_inspection`
//! 同形但状态守卫不同。
//!
//! 2026-09-11 part/assembly/batch 重构方案 §4.2 (PR-B2)：删除 9 个「无业务调用
//! 方」旧函数（`count_other_inprocess_batches` / `find_most_recent_batch_for_part`
//! / `mark_part_passed_inspection` / `mark_part_inspected` /
//! `mark_part_failed_inspection` / `mark_part_returned` / `mark_part_delivered`
//! / `mark_part_completed` / `mark_part_repairing`）—— 工单流转现在统一走
//! `PartService::sync_from_batch_change` rollup，不直接写 `t_part`。
//!
//! 2026-09-16 PR-3 批次 step 化（migration 028）：
//! - 所有 TPartBatch 投影删 `next_process_id` / `placed_at`，加 `current_process_step_id`
//! - `mark_batch_failed_inspection` / `mark_batch_returned` 参数改：
//!   - 旧：`next_process_id: i64`（写入 batch.next_process_id 列）
//!   - 新：`current_process_step_id: Option<i64>`（写入 batch.current_process_step_id 列；
//!     step_id 由 caller 在 phase1 service 中按 chain_id + process_id 解析后传入）

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
