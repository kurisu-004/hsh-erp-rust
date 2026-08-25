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
//! - `mark_part_passed_inspection` —— pass_inspection 后同步工单状态（OCC UPDATE `t_part.status`）
//! - `count_other_inprocess_batches` —— 多轮 rollup 守卫（>0 时不翻 `t_part.status`）
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

    /// 定位 part 的 INSPECTION 状态批次。
    ///
    /// - `expected_batch_id = None`：按 `(part_id, status = 'INSPECTION')` 唯一解析。
    ///   先 `COUNT(*)` 校验唯一性（≥2 → 歧义，返回 `Err(sqlx::Error::RowNotFound)`，
    ///   service 层映射为 `20109 / BIZ_PART_BATCH_NOT_FOUND`）；== 0 → `Ok(None)`；
    ///   == 1 → 取 id 最小者（与既有按 batch 序号的语义一致）。
    ///   唯一性守卫原因：`split_batch_for_partial_pass` 可能产生多个 INSPECTION
    ///   子批次；没有 caller 给 `expected_batch_id` 时，旧实现 `ORDER BY id LIMIT 1`
    ///   会静默选最低 id，可能送检错误批次。
    /// - `expected_batch_id = Some(bid)`：按 id 校验 ownership（防止 caller 误传
    ///   其它 part 的 batch id）。
    ///
    /// 两路径均要求 `deleted_at IS NULL`。
    ///
    /// 签名收 `&mut PgConnection`：方法在 `None` 分支需在同一事务内连发两条 SQL
    /// （COUNT + SELECT），与 `split_batch_for_partial_pass` 同形 —— sqlx 0.9 没有
    /// `Executor` 的 `&T where T: Executor` blanket impl，generic `E: PgExecutor<'e>`
    /// 不能 move 两次。
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
                // 1. 唯一性守卫：COUNT(*) 检查 INSPECTION 批次数量。
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
                        // 2. 唯一命中：取 id 最小者。
                        sqlx::query_as!(
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
                        .await
                    }
                    _ => {
                        // ≥2 个 INSPECTION 批次：歧义。Service 层负责把
                        // `sqlx::Error::RowNotFound` 翻译成 `AppError::Biz` /
                        // `20109 / BIZ_PART_BATCH_NOT_FOUND`。
                        Err(sqlx::Error::RowNotFound)
                    }
                }
            }
        }
    }

    /// 批量通过（OCC UPDATE）。
    ///
    /// - 0 行 → 版本冲突 / 状态非 INSPECTION / 已软删 —— 由 service 层映射为 `40901`。
    /// - 成功 → `status = 'READY_TO_SHIP'`，`version += 1`，`updated_at = now()`，
    ///   `updated_by = current_user_id`。
    ///
    /// `current_user_id` 写入审计列 `t_part_batch.updated_by`（与 split 等
    /// 其它写入路径保持一致；nullable 以兼容 caller 不持有的场景）。
    ///
    /// **未触碰 `t_part` 行**：工单（`t_part`）的状态翻转由 service 层在事务内
    /// 紧接着调用 [`Self::mark_part_passed_inspection`] 完成 —— 这样
    /// `PartOut.status` 在接口响应时就能反映最新状态（避免上层读到的 status
    /// 还是 `INSPECTION`）。
    ///
    /// 注：`t_part_batch` 没有 `actual_delivery_date` 列（migration 006 校验过）；
    /// 该列只存在于 `t_part` / `t_assembly`。
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
    /// - 0 行 → 工单已被并发修改（version 不匹配）/ 状态非 INSPECTION /
    ///   已软删 —— 由 service 层映射为 `40901 / VERSION_CONFLICT`。
    /// - 成功 → `status = 'READY_TO_SHIP'`，`version += 1`，`updated_at = now()`，
    ///   `updated_by = current_user_id`。
    ///
    /// `current_user_id` 写入审计列 `t_part.updated_by`，与 split 等其它
    /// 写入路径保持一致（nullable 兼容 caller 不持有的场景）。
    ///
    /// **必须与 [`Self::mark_batch_passed_inspection`] 在同一事务内调用**，否则
    /// 可能出现 batch 已翻 READY_TO_SHIP 但工单仍 INSPECTION 的不一致窗口。
    ///
    /// **多轮部分送检 rollup**：本方法只在 `service::pass_inspection_core`
    /// 确认 `count_other_inprocess_batches == 0` 后才调用 —— 否则部分批次
    /// 送检通过会把工单错误地翻成 `READY_TO_SHIP`，而其它 INSPECTION 批次
    /// 仍存在（对齐 Python `_rollup_part_status` 的 `least(batches.status)`
    /// 语义）。
    ///
    /// 设计取舍：本方法只翻 `status` + `version`，不更新 `location` /
    /// `current_holder_id` / `next_process_id`。Python `service/_pass_inspection.py`
    /// 的 `_rollup_part_status` 会做更完整的字段同步，但本次 PR 仅对齐状态字段
    /// （满足 `PartOut.status` 不陈旧的契约）。location / holder 同步留待
    /// 后续 PR（Task 2 之后）单独实施，避免越界改动既有 pass_inspection 流。
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

    /// 统计 part 仍处于 INSPECTION 状态的非软删批次数量。
    ///
    /// 用于 `pass_inspection_core` 多轮 rollup 守卫：每次送检一批通过后，
    /// 在翻 `t_part.status` 前调用本方法，若结果 > 0 则说明还有其它 INSPECTION
    /// 批次存在，工单必须保留 `INSPECTION` 状态（与 Python `_rollup_part_status`
    /// `least(batches.status)` 语义对齐）。返回 `i64`（而非 `bool`）便于
    /// caller 后续做更复杂的 rollup（如记录 metric）。
    ///
    /// 注：本方法只计 INSPECTION，不包含其它在途状态（如已被并发翻成 READY_TO_SHIP
    /// 的批次的回归检测）。若需要全部在途状态计数，迁移 schema 时再加。
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

    /// 定位 scan-inspect 的目标批次（白名单 `{PENDING, PROGRAMMING, IN_PROCESS}`）。
    ///
    /// 与 `find_inprocess_batch_for_part` 同形：
    /// - `expected_batch_id = None`：先 COUNT 校验唯一性（≥2 → `RowNotFound`）；
    ///   == 0 → `Ok(None)`；== 1 → 取 id 最小者。
    /// - `expected_batch_id = Some(bid)`：按 id 校验 ownership + status in 白名单。
    ///
    /// 签名收 `&mut PgConnection`（与既有同形）。
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
    ///
    /// 与 `find_inprocess_batch_for_part` 同形（白名单仅 `{INSPECTION}`）。
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

    /// scan-inspect 第一步：工单搬到品检架（OCC UPDATE t_part）。
    ///
    /// 0 行 → 版本冲突 / 状态不在 scan 白名单 → 40901 VERSION_CONFLICT。
    /// 成功 → status='INSPECTION', location='INSPECTION_SHELF', current_holder_id=shelf_id,
    /// version += 1。
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
    ///
    /// 与 `mark_part_inspected` 同事务调用；t_part_batch 已有 `location` /
    /// `current_holder_id` 字段。
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
    ///
    /// 0 行 → 40901 VERSION_CONFLICT。
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
    ///
    /// 与 `mark_batch_failed_inspection` 同事务调用。
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
}
