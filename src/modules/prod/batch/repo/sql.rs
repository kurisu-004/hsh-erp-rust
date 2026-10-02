//! `t_part_batch` inspection / lifecycle 流转的定位 + 写点（`impl PartBatchRepo`）。
//!
//! 2026-10-02 随 `t_part_batch` 归属迁入 prod 域：文件自
//! `part/repo/sql/batch_sql.rs` 搬来，impl 目标由 part 域 ZST `PartRepo` 改为
//! 本域 ZST `PartBatchRepo`（`t_part_batch` 的 SQL 真源在 `queries.rs`）。
//!
//! ## 承载方法（19 个）
//!
//! ### find_*（7）
//! - `find_inprocess_batch_for_part` / `find_scan_target_batch`
//! - `find_inspection_batch_by_id` / `find_current_inspection_batch_id`
//! - `find_batch_by_id` / `find_inprocess_batch_by_id_and_holder`
//! - `find_worker_held_batch_for_part`
//!
//! ### mark_*_inspection / mark_batch_returned（4）
//! - `mark_batch_passed_inspection` / `mark_batch_inspected`
//! - `mark_batch_failed_inspection` / `mark_batch_returned`
//!
//! ### mark_*_lifecycle / cancel_all（6）
//! - `mark_batch_delivered` / `mark_batch_completed`
//! - `mark_part_cancelled` / `mark_batch_cancelled`
//! - `mark_batch_repairing` / `cancel_all_active_batches_for_part`
//! - （另有 `force_complete_all_batches_for_part`）
//!
//! ### split（1）
//! - `split_batch_for_partial_pass`（薄包装 `PartBatchRepo::_split_batch_inner`）
//!
//! ## 2026-10-02 去 part 化
//! `find_inspection_batch_for_fail` 更名 `find_inspection_batch_by_id` 并**去掉
//! `part_id` 形参**：调用方是 `to_process_core`，而 `batch_id` 自 2026-10-02 起
//! 是 URL 路径参数（`POST /prod/batches/{batch_id}/to-process`），必填 ⇒ 原来的
//! 「COUNT-then-SELECT 消歧」`None` 分支成为死代码，一并删除；`part_id` 由
//! service 从批次行反查，SQL 里的 `AND part_id = $2` 恒真，属冗余断言。
//!
//! ## 事务 / 错误类型
//! 2026-10-01 起除 `mark_batch_returned`（只写 holder/location，不改 status）外，
//! 所有 `t_part_batch.status` 写点都是 `status_gate` 之上的薄包装 —— 全仓唯一的
//! 批次状态写入口。

use sqlx::{PgConnection, PgExecutor};

use super::queries::PartBatchRepo;
use crate::modules::prod::batch::model::TPartBatch;
use crate::modules::prod::batch::status_gate::{self, StatusChange};
use crate::shared::error::AppError;

impl PartBatchRepo {
    /// 定位 INSPECTION 状态批次。
    ///
    /// - `expected_batch_id = None`：先 COUNT 校验唯一性（≥2 → 歧义 `RowNotFound`），
    ///   == 0 → `Ok(None)`，== 1 → 取 id 最小者。
    /// - `expected_batch_id = Some(bid)`：按 id + 状态定位。
    ///
    /// 签名收 `&mut PgConnection`：方法在 `None` 分支需在同一事务内连发两条 SQL。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加 current_process_step_id。
    ///
    /// 2026-10-02：`Some` 分支删 `AND part_id = $2`。调用方是 `to_ship_core`，其
    /// `part_id` 由同一批次行反查得到 ⇒ 该谓词恒真，属冗余断言（批次 id 全局唯一）。
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
                       current_holder_id, current_process_id, current_process_step_id,
                       delivery_note_id, parent_batch_id,
                       is_repairing,
                       version, created_at, created_by, updated_at, updated_by,
                       deleted_at
                FROM t_part_batch
                WHERE id = $1 AND status = 'INSPECTION'
                  AND deleted_at IS NULL
                "#,
                    bid,
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
                               current_holder_id, current_process_id, current_process_step_id,
                               delivery_note_id, parent_batch_id,
                               is_repairing,
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
    ///
    /// 2026-10-02：`Some` 分支删 `AND part_id = $2`（调用方 `to_inspection_core`
    /// 的 `part_id` 由同一批次行反查 ⇒ 恒真）。
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
                       current_holder_id, current_process_id, current_process_step_id,
                       delivery_note_id, parent_batch_id,
                       is_repairing,
                       version, created_at, created_by, updated_at, updated_by,
                       deleted_at
                FROM t_part_batch
                WHERE id = $1
                  AND status IN ('PENDING', 'PROGRAMMING', 'IN_PROCESS')
                  AND deleted_at IS NULL
                "#,
                    bid,
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
                               current_holder_id, current_process_id, current_process_step_id,
                               delivery_note_id, parent_batch_id,
                               is_repairing,
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

    /// 按 id 定位 INSPECTION 状态批次（`to-process` 锚点校验）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加 current_process_step_id。
    ///
    /// 2026-10-02：原 `find_inspection_batch_for_fail(part_id, Option<batch_id>)`
    /// 改写为按 id 定位。`batch_id` 自 `POST /prod/batches/{batch_id}/to-process`
    /// 起是必填的
    /// 路径参数 ⇒ ① `part_id` 形参删除（part_id 由 service 从批次行反查，SQL 里
    /// `AND part_id = $2` 恒真）；② `None` 分支（COUNT-then-SELECT 的多候选消歧
    /// 守卫）成为死代码，一并删除 —— 唯一调用方是 `to_process_core`，它恒传
    /// `Some(batch_id)`。
    pub async fn find_inspection_batch_by_id(
        conn: &mut PgConnection,
        batch_id: i64,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, current_process_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   is_repairing,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at
            FROM t_part_batch
            WHERE id = $1 AND status = 'INSPECTION'
              AND deleted_at IS NULL
            "#,
            batch_id,
        )
        .fetch_optional(&mut *conn)
        .await
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
                   current_holder_id, current_process_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   is_repairing,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_part_batch
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            batch_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 批量通过（OCC UPDATE）—— **2026-10-01 起为 status_gate 薄包装**。
    ///
    /// 签名两处变更（调用方零改动，全部经 `&mut **self` 传连接）：
    /// - `executor: E` → `conn: &mut PgConnection`：status_gate 的派生步骤
    ///   需要可变的 `PgConnection`（D-6 架构：service 不持连接，跨域调用经
    ///   `repo.conn_mut()`），泛型 `PgExecutor` 表达不了。
    /// - `Result<u64, sqlx::Error>` → `Result<RollupOutcome, AppError>`：
    ///   ① `sqlx::Error` → `AppError`：status_gate 的契约是「没写成 =
    ///   `VERSION_CONFLICT`」，转成 `sqlx::Error` 会把 409 降级成 500；
    ///   ② `u64` → `RollupOutcome`：**2026-10-01 修正**。status_gate 一函数内
    ///   已完成 part 派生 + assembly 反向同步，而调用点（`inspection_core.rs`
    ///   的 3 处）需要 `SyncOutcome` 填响应的 `synced_assembly_id`、并据此发
    ///   `ASSEMBLY_UPDATED` 广播。若这里只回 `u64`、让 service 再调一次
    ///   `PartService::sync_from_batch_change`，第二次派生必然 `NoChange`
    ///   （target 已 == 当前），`synced_assembly_id` 会被**恒为 null** 吞掉。
    ///   故把 gate 的派生结果原样透出；0 行仍由 gate 直接抛 `VERSION_CONFLICT`。
    pub async fn mark_batch_passed_inspection(
        conn: &mut PgConnection,
        batch_id: i64,
        expected_version: i32,
        current_user_id: Option<i64>,
    ) -> Result<status_gate::RollupOutcome, AppError> {
        status_gate::apply_batch_status_change_detailed(
            conn,
            StatusChange {
                batch_id,
                new_status: "READY_TO_SHIP",
                new_location: None,
                new_holder_id: None,
                new_process_id: None,
                new_process_step_id: None,
                is_repairing: None,
                expected_version: Some(expected_version),
                allowed_from: &["INSPECTION"],
                // 全部生产调用方都传 `Some(current.id)`（`inspection_core.rs`
                // 的 3 处）；`None` 分支保留为 0，与 `update_batch_fields` 的
                // `COALESCE($3, ...)` 历史容忍度一致。
                updated_by: current_user_id.unwrap_or(0),
                // 2026-10-01 review 第 1 轮 M2：本包装函数的 `None` 一律是
                // 「保持原值」，清空语义由同名 clear_* 显式表达。
                clear_location: false,
                clear_holder_id: false,
                clear_process_id: false,
                clear_process_step_id: false,
                // 2026-10-01 review 第 1 轮 M4：归档事件 id 由 caller 透传。
                event_id: None,
            },
        )
        .await
    }

    /// to-inspection 第一步：批次状态同步（OCC UPDATE t_part_batch）。
    ///
    /// 2026-09-30（review 第 1 轮 H2 修复）本函数补写 `current_process_id = NULL`：
    /// 送检是**出池**（写入不变式第 2 行：出池 → 置 NULL）。此前本函数只翻
    /// `status` / `location` / `current_holder_id`，批次会带着上一道工序的
    /// `current_process_id` 停在 `INSPECTION` 状态 —— 与 migration 004 的
    /// COLUMN COMMENT「NULL 表示批次不在生产工序池中」矛盾。
    ///
    /// 之所以**不会**立刻造成池污染：全部 4 条工序池 SQL
    /// （`take_one_from_pool` / `list_candidates_by_process_all_shelves` /
    /// `group_count_by_process_all_shelves` / `count_pool_by_shelf_and_process`）
    /// 与 `work_type::list_pickable_by_work_type` 都同时限定
    /// `status='IN_PROCESS' AND location='PRODUCTION_SHELF'`。但那条 DB 级不变量
    /// 必须成立，否则将来任何「只按 `current_process_id` 过滤、不带
    /// status/location」的查询都会把送检批次错当池内批次捞出来。
    ///
    /// 关于 `current_process_step_id`：**本函数仍不写它**（沿用 2026-09-16 PR-3
    /// 行为）。原先的注释理由是「保留被打回的那一步，让 INSPECTION→to_process
    /// 时不丢 step 上下文」—— 该理由在 step 降级为**可选的显示用定位信息**后已不成立：
    /// `mark_batch_failed_inspection`（检验不合格打回生产架）会按
    /// `chain_id + next_process_id` **重新解析** step_id 写入
    /// （`inspection_core.rs::to_process`），所以上下文不会真的丢。
    ///
    /// 现在保留 step 的实际价值：它是 `GET /parts/inspection-batches` 的
    /// `next_process_id` / `next_process_name` 的**唯一数据来源**（step JOIN 派生），
    /// 供送检期间前端显示批次**首次定位**在工艺链的哪一步。属**显示用信息**，
    /// 不是状态机依赖。
    /// ⚠️ 措辞订正（2026-09-30 review 第 3 轮附带发现）：**不是**「当前走到第
    /// 几步」—— 本列只在首次定位工序时写、之后一律不再推进（worker-scan
    /// RETURNED / INSPECTED 都不写），对多工序链工单永远停在首次定位那一步。
    pub async fn mark_batch_inspected(
        conn: &mut PgConnection,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<status_gate::RollupOutcome, AppError> {
        status_gate::apply_batch_status_change_detailed(
            conn,
            StatusChange {
                batch_id,
                new_status: "INSPECTION",
                new_location: Some("INSPECTION_SHELF"),
                new_holder_id: Some(shelf_id),
                new_process_id: None,
                // 2026-09-16 PR-3：to_inspection 保留 current_process_step_id，
                //   但其定位已降级为「可选的显示用定位信息」（首次定位后不再推进）；
                //   to_process 会重新解析 step 写入，故此处不写不丢状态机上下文。
                new_process_step_id: None,
                is_repairing: None,
                expected_version: Some(expected_version),
                allowed_from: &["PENDING", "PROGRAMMING", "IN_PROCESS"],
                updated_by: current_user_id.unwrap_or(0),
                // 2026-09-30（review H2）：出池 → 池归属权威列必须置 NULL
                //   （不置 NULL 会让 INSPECTION 批次带着上一道工序 id 停留）。
                clear_process_id: true,
                // 2026-10-01 review 第 1 轮 M2：本包装函数的 `None` 一律是
                // 「保持原值」；送检刻意**保留** `current_process_step_id`
                //（INSPECTION 期间要显示批次走到工艺链第几步，见
                //  `part/vo/inspection.rs`），故 step 的 clear 为 false。
                clear_location: false,
                clear_holder_id: false,
                clear_process_step_id: false,
                event_id: None,
            },
        )
        .await
    }

    /// to-process：批次打回生产架（OCC UPDATE t_part_batch）。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：
    /// - 参数 `next_process_id: i64` 改 `current_process_step_id: Option<i64>`
    /// - 写入列：t_part_batch.next_process_id（已删）→ t_part_batch.current_process_step_id
    /// - step_id 由 phase1 service 在调用本函数前按 `chain_id + process_id` 解析后传入
    ///
    /// 2026-09-30 新增 `current_process_id: Option<i64>`：检验不合格打回生产架
    /// = **进池**，故写入目标工序（池归属权威依据）；`current_process_step_id`
    /// 仍是可选的显示用定位信息（首次定位后不再推进），允许 NULL。
    ///
    /// ## `is_repairing: None`（保持）的合法性前提（2026-10-01 review 第 2 轮 MAJOR-3）
    ///
    /// 唯一 caller 是 `inspection_core::to_process_core`，它在 step 4.6 **拒绝**
    /// `is_repairing = true` 的批次（返修件走 `complete-repair` 闭环）。故本函数
    /// 看到的批次恒为 `is_repairing = false`，「保持」与「清 false」等价。
    ///
    /// 之所以仍写 `None`（保持）而不是 `Some(false)`：若将来新增 caller 忘了那条
    /// 守卫，`Some(false)` 会**静默**把返修件挪出返修流（正是 MAJOR-3 的失败类别），
    /// 而 `None` 会让同一个洞以「标记与状态矛盾」的形式暴露在
    /// `GET /parts/repairing-batches` 上，更容易被发现。
    pub async fn mark_batch_failed_inspection(
        conn: &mut PgConnection,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_process_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<status_gate::RollupOutcome, AppError> {
        status_gate::apply_batch_status_change_detailed(
            conn,
            StatusChange {
                batch_id,
                new_status: "IN_PROCESS",
                new_location: Some("PRODUCTION_SHELF"),
                new_holder_id: Some(shelf_id),
                new_process_id: current_process_id,
                new_process_step_id: current_process_step_id,
                is_repairing: None,
                expected_version: Some(expected_version),
                allowed_from: &["INSPECTION"],
                updated_by: current_user_id.unwrap_or(0),
                // 进池 → 写目标工序；`current_process_id` 为 None 时即「不进任何
                // 工序池」，清 NULL 是本路径的既有语义。
                clear_process_id: current_process_id.is_none(),
                // 2026-10-01 review 第 1 轮 M2：location / holder 都有实参，
                // step 为 `None` 时是「保持原值」（本路径的既有语义）。
                clear_location: false,
                clear_holder_id: false,
                clear_process_step_id: false,
                event_id: None,
            },
        )
        .await
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
                   current_holder_id, current_process_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   is_repairing,
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
    ///   `version += 1`。
    ///
    /// `current_user_id` 写入 `updated_by`（nullable 与既有路径一致）。
    ///
    /// ## 2026-09-16 PR-3 批次 step 化（migration 028）
    ///
    /// - 参数 `next_process_id: i64` 改 `current_process_step_id: Option<i64>`
    /// - 写入列改为 t_part_batch.current_process_step_id
    ///
    /// ## 2026-09-30 重构（prod/pool move 合并 remove 路径）
    ///
    /// - 移除 `current_process_step_id` SET 子句；admin 主动退回只是把 holder
    ///   切回 pool，不推进工序链。
    /// - 形参 `current_process_step_id` 保留 `_` 前缀 —— **它被丢弃**（根本没进
    ///   `query!` 的 bind 列表）。详见下面「已知缺口」段。
    ///
    /// ## 2026-09-30（review 第 1 轮 H1 修复）新增 `advance_to_process_id`
    ///
    /// 本函数此前**既不写** `current_process_step_id` **也不写**
    /// `current_process_id`，但它有两个语义相反的调用方：
    ///
    /// | 调用方 | 语义 | `advance_to_process_id` |
    /// |---|---|---|
    /// | `worker_scan.rs::worker_scan` RETURNED | **推进工序**：工人在 P1 完工、扫 RETURNED 传 `next_process_id=P2`，批次应落进 **P2** 池 | `Some(P2)` |
    /// | `prod/worker_pool/service.rs::move_batch` WORKER→POOL | **池内移动**：工种不变，批次归还货架后仍属原工序候选池 | `None` |
    ///
    /// 修复前 RETURNED 路径不写该列 → 批次带着 `current_process_id=P1` 归还货架
    /// → 落回 **P1** 池而非 P2 池。这正是 migration 004 要确立的「唯一权威依据」
    /// 在主干流程（完工归还）上说谎；且本次修复打破了「要推进 step 先进池、要进池
    /// 先有 step」的死锁后，RETURNED 从不可达变为可达，该路径的问题会立刻暴露。
    ///
    /// **采用 `None` = 不改（COALESCE）而非拆两个函数**，理由：
    /// 1. WHERE 守卫（`status='IN_PROCESS' AND location='WORKER'` + OCC
    ///    `version=$2`）是安全关键，拆两份就变成两份必须手工保持同步的守卫，
    ///    漂移即等于状态机被绕过；
    /// 2. 仓内已有同形先例：`PartRepo::update_batch_fields` 的
    ///    `COALESCE($3::bigint, delivery_note_id)`；
    /// 3. 两个调用点各只有 1 处，可读性收益小，而 SQL 守卫重复的收益为负。
    ///
    /// 语义靠形参名 + 本 doc 锁定：`None` 是「**不推进**」，**不是**「清空为
    /// NULL」。本函数没有任何调用方需要「清空」—— 清空属于出池，走
    /// `mark_batch_with_status_and_meta`（漏斗）或 `mark_batch_inspected`。
    ///
    /// ## 已知缺口（2026-09-30 记录，本轮不扩 scope）
    ///
    /// `current_process_step_id`（可选的显示用定位信息）**在 RETURNED 时不推进**：
    /// `worker_scan.rs:193-204` 已经把 `chain_id + next_pid` 解析成 `step_id_opt`，
    /// 却传给一个被丢弃的形参。
    /// 影响面仅限显示：池归属已由 `current_process_id` 承担且本函数已正确写入。
    /// 待后续单独一轮处理（届时 `mark_batch_returned` 需要按调用方决定是否写
    /// step，语义与 `advance_to_process_id` 同形）。
    ///
    /// ⚠️ 措辞订正（2026-09-30 review 第 3 轮附带发现）：本列**不是会随流转推进的
    /// 「进度指针」**，它只在**首次定位**工序时被写入（dispatch 刻意写 NULL；其余
    /// 由 place_on_shelf / release_from_programming / outsource 收发 /
    /// complete_repair / to_process 写），**之后一律不再推进**。对多工序链工单它
    /// 永远停在首次定位那一步。修这一缺口时应把它当「一次性定位 + 可选重定位」看，
    /// 而不是「每流转一步就前进一步」。
    pub async fn mark_batch_returned<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        _current_process_step_id: Option<i64>,
        advance_to_process_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET current_holder_id       = $3,
                location                = 'PRODUCTION_SHELF',
                -- 2026-09-30（review H1）：条件写入 —— Some(目标 process_id) 推进
                --   工序（worker-scan RETURNED，批次落进下一道工序的候选池）；
                --   None 表示「不推进」（prod/pool move 池内移动，工序不变）。
                --   用 COALESCE 而非直接 `= $5`，是因为直接赋值会在 move 路径
                --   把 current_process_id 抹成 NULL（那会让归还的批次对所有池隐身）。
                current_process_id      = COALESCE($5::bigint, current_process_id),
                -- 2026-09-30：current_process_step_id 仍不写（可选的显示用定位信息，
                --   RETURNED 不推进是已知缺口，见函数 doc「已知缺口」段）
                version                 = version + 1,
                updated_at              = now(),
                updated_by              = $4
            WHERE id = $1 AND version = $2
              AND status = 'IN_PROCESS' AND location = 'WORKER'
              AND deleted_at IS NULL
            "#,
            batch_id,
            expected_version,
            shelf_id,
            current_user_id as Option<i64>,
            advance_to_process_id,
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
                       current_holder_id, current_process_id, current_process_step_id,
                       delivery_note_id, parent_batch_id,
                       is_repairing,
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
                               current_holder_id, current_process_id, current_process_step_id,
                               delivery_note_id, parent_batch_id,
                               is_repairing,
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
    /// 2026-09-17 PR-4 卫生项 B2：薄包装委托到 `_split_batch_inner`
    /// （prod/batch/repo/queries.rs 的 `PartBatchRepo`）；`split_batch`（手动部分量）也
    /// 委托同一 helper。
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
        super::queries::PartBatchRepo::_split_batch_inner(
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

    /// 批次 READY_TO_SHIP → DELIVERED —— **2026-10-01 起为 status_gate 薄包装**。
    pub async fn mark_batch_delivered(
        conn: &mut PgConnection,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, AppError> {
        status_gate::apply_batch_status_change(
            conn,
            StatusChange {
                batch_id,
                new_status: "DELIVERED",
                new_location: None,
                new_holder_id: None,
                new_process_id: None,
                new_process_step_id: None,
                is_repairing: None,
                expected_version: Some(expected_version),
                allowed_from: &["READY_TO_SHIP"],
                updated_by: current_user_id,
                // 2026-10-01 review 第 1 轮 M2：本包装函数的 `None` 一律是
                // 「保持原值」，清空语义由同名 clear_* 显式表达。
                clear_location: false,
                clear_holder_id: false,
                clear_process_id: false,
                clear_process_step_id: false,
                // 2026-10-01 review 第 1 轮 M4：归档事件 id 由 caller 透传。
                event_id: None,
            },
        )
        .await
        .map(|_| 1u64)
    }

    /// 批次 DELIVERED → COMPLETED —— **2026-10-01 起为 status_gate 薄包装**。
    ///
    /// 本函数是「part 被 rollup 进 COMPLETED → 归档并释放 `serial_no`」这条
    /// 新链路的**最常见触发点**：part 只有在所有非取消批次都 COMPLETED 时才会
    /// 派生到 COMPLETED，故多批次工单完成最后一条时自动释放，无需 service 层
    /// 再记得调 `clear_part_serial_no_when_completed`。
    ///
    /// `event_id`（2026-10-01 review 第 1 轮 M4）：本函数是**能让 part 新进
    /// COMPLETED** 的写点之一，故必须由 caller 传一个真实雪花 id 供终态序列号
    /// 归档事件（`SERIAL_RELEASED`）使用。
    pub async fn mark_batch_completed(
        conn: &mut PgConnection,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
        event_id: Option<i64>,
    ) -> Result<u64, AppError> {
        status_gate::apply_batch_status_change(
            conn,
            StatusChange {
                batch_id,
                new_status: "COMPLETED",
                new_location: None,
                new_holder_id: None,
                new_process_id: None,
                new_process_step_id: None,
                is_repairing: None,
                expected_version: Some(expected_version),
                allowed_from: &["DELIVERED"],
                updated_by: current_user_id,
                // 2026-10-01 review 第 1 轮 M2：本包装函数的 `None` 一律是
                // 「保持原值」，清空语义由同名 clear_* 显式表达。
                clear_location: false,
                clear_holder_id: false,
                clear_process_id: false,
                clear_process_step_id: false,
                // 2026-10-01 review 第 1 轮 M4：终态归档事件 id 由 caller 透传。
                event_id,
            },
        )
        .await
        .map(|_| 1u64)
    }

    /// 工单取消（OCC UPDATE t_part）：白名单 5 状态（PENDING / PROGRAMMING /
    /// INSPECTION / READY_TO_SHIP / DELIVERED），清空 `serial_no`。
    ///
    /// ## 2026-10-01：**本函数刻意不走 status_gate**
    ///
    /// status_gate 的写入口是 `t_part_batch.status`（**批次**是状态的真源，
    /// part / assembly 是派生缓存）。本函数写的是 `t_part.status` —— 它是
    /// 「用户取消工单」这个**主操作**本身，不是派生写：cancel 的合法源状态
    /// 白名单与批次流无关（一个未拆批的工单也要能取消），且必须在**批次级联
    /// 取消之前**先把 part 打成终态，才能挡住并发的批次流转。
    ///
    /// `serial_no = NULL` 同理是**故意不归档**：cancel 是「作废」，序列号就此
    /// 退役，不是「转交送货单后释放复用」。子件的归档释放只发生在
    /// COMPLETED（见 `status_gate::release_part_serial_no`）。
    ///
    /// 级联取消全部活跃批次由 `cancel_all_active_batches_for_part` 走
    /// status_gate 的 bulk 模式完成（会自动补做 part → assembly 派生）。
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

    /// 批次取消（OCC UPDATE t_part_batch）：白名单 5 状态
    /// —— **2026-10-01 起为 status_gate 薄包装**。
    ///
    /// `event_id`：本函数能让 part 新进 CANCELLED（它是该 part 最后一条活跃
    /// 批次时），故按 M4 由 caller 透传归档事件雪花 id。
    pub async fn mark_batch_cancelled(
        conn: &mut PgConnection,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
        event_id: Option<i64>,
    ) -> Result<u64, AppError> {
        status_gate::apply_batch_status_change(
            conn,
            StatusChange {
                batch_id,
                new_status: "CANCELLED",
                new_location: None,
                new_holder_id: None,
                new_process_id: None,
                new_process_step_id: None,
                is_repairing: None,
                expected_version: Some(expected_version),
                allowed_from: &[
                    "PENDING",
                    "PROGRAMMING",
                    "INSPECTION",
                    "READY_TO_SHIP",
                    "DELIVERED",
                ],
                updated_by: current_user_id,
                // 2026-10-01 review 第 1 轮 M2：本包装函数的 `None` 一律是
                // 「保持原值」，清空语义由同名 clear_* 显式表达。
                clear_location: false,
                clear_holder_id: false,
                clear_process_id: false,
                clear_process_step_id: false,
                // 2026-10-01 review 第 1 轮 M4：终态归档事件 id 由 caller 透传。
                event_id,
            },
        )
        .await
        .map(|_| 1u64)
    }

    /// 批次 IN_PROCESS → REPAIRING。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：t_part_batch 删 `has_been_repaired`
    /// 列；返修事实由 `t_part_event` REPAIR_STARTED 事件追溯，本函数不再
    /// 写返修标。
    ///
    /// **已知局限（2026-09-30 review 第 3 轮 M4 (4d)，已决策，本次不改 SQL）**：
    /// 本函数把批次翻出 `IN_PROCESS`（按出池不变式应清 `current_process_id`）却
    /// **既不写也不清**该列，location 仍是 `PRODUCTION_SHELF`。
    ///
    /// 已决策（用户拍板，2026-09-30）：
    /// 1. **正常业务流不存在 `IN_PROCESS → REPAIRING` 转换** —— 现实中走不到，
    ///    故不产生残留脏数据；
    /// 2. `REPAIRING` 后续**不再作为状态机状态，仅作标记（flag）**，`status`
    ///    取值与相关状态判定届时整体重做；
    /// 3. 故**本次不改 SQL**：无实际数据后果，且 `REPAIRING` 语义即将变更，
    ///    此刻补 `current_process_id = NULL` 属白改，还会在降级重构时造成一次
    ///    无谓回改；
    /// 4. 本写点连同 `part_status_progress()`（`IN_PROCESS | REPAIRING => 2`
    ///    同档）与 `PartStatus::can_transition_to` 中以 REPAIRING 为端点的迁移，
    ///    在 `REPAIRING` 降级为标记的那次重构中**一并处理**。
    ///
    /// 详见 `migrations/20260930000000_004_add_batch_current_process_id.sql`
    /// 「已知局限 (4)」小节。
    ///
    /// ## 2026-10-01 语义变更：不再改 status，改置 `is_repairing` 标记
    ///
    /// REPAIRING 已从 `PartStatus` 降级为标记（migration 005/006）：返修仍在
    /// 生产中，故 `status` 保持 `IN_PROCESS`（progress 与原 REPAIRING 同档
    /// 2，rollup 结果不变），「是否在返修」改由 `is_repairing` 承载。
    ///
    /// 这同时**消掉了** migration 004「已知局限 (4d)」记的那个残留写点：本函数
    /// 不再把批次翻出 `IN_PROCESS`，`current_process_id` 也就没有「该清未清」
    /// 的问题（`new_process_id: None` + `clear_process_id: false` = 不动）。
    pub async fn mark_batch_repairing(
        conn: &mut PgConnection,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, AppError> {
        status_gate::apply_batch_status_change(
            conn,
            StatusChange {
                batch_id,
                new_status: "IN_PROCESS",
                new_location: None,
                new_holder_id: None,
                new_process_id: None,
                new_process_step_id: None,
                is_repairing: Some(true),
                expected_version: Some(expected_version),
                allowed_from: &["IN_PROCESS"],
                updated_by: current_user_id,
                // 2026-10-01 review 第 1 轮 M2：本包装函数的 `None` 一律是
                // 「保持原值」，清空语义由同名 clear_* 显式表达。
                clear_location: false,
                clear_holder_id: false,
                clear_process_id: false,
                clear_process_step_id: false,
                // 2026-10-01 review 第 1 轮 M4：归档事件 id 由 caller 透传。
                event_id: None,
            },
        )
        .await
        .map(|_| 1u64)
    }

    /// 2026-09-11 part/assembly/batch 重构方案 §4.2 (PR-B2)：part cancel 时
    /// 级联取消**全部活跃批次**（不只「最近一条 source-status」）。
    ///
    /// ## 2026-10-01：两处修正
    ///
    /// **修正 1 —— 补状态白名单（真 bug）**：改造前的 WHERE 只有
    /// `part_id=$1 AND deleted_at IS NULL`，**没有 status 过滤**，会把 part 下
    /// 已经 COMPLETED 的批次一起拖成 CANCELLED。已完成的货被改写成「已作废」，
    /// 之后按序列号 / 送货单回溯全都对不上，且该行不可逆（终态被改写）。此处补
    /// `NOT (status IN ('COMPLETED','CANCELLED'))`。
    ///
    /// **修正 2 —— 走 status_gate bulk 模式**：改写完自动对受影响的每个 part
    /// 补做**父装配件**派生（改造前 `PartService::cancel` 压根不调任何 sync，
    /// 父装配件的派生状态靠下一次任意 part 流转才追平）。
    ///
    /// **修正 3 —— `PartDerivation::KeepPartTerminalAsIs`**（2026-10-01
    /// review 第 1 轮 B1）：本函数在 `mark_part_cancelled` **之后**调用，此时
    /// `t_part.status` 已由主操作写成 CANCELLED。而 min-progress 在「已完成批次
    /// 而「其余被批量取消」时会算出 COMPLETED —— 若放任派生写，用户的「作废工单」
    /// 会被静默改回 COMPLETED（接口 200、事件流水记 CANCELLED、界面显示已完成、
    /// 序列号已清空可被复用），父装配件还会被级联推成 COMPLETED。故这里显式
    /// 声明「part 已是终态，一个字都不许碰」，只继续派生父层。
    ///
    /// 终态守卫（`update_part_rollup` 的 `status NOT IN (...)`）是同一不变式的
    /// SQL 层兜底，两处都要在：守卫拦住的是「派生写覆盖终态」，本策略额外保证
    /// 「跳过 part 写的同时父装配件仍被派生追平」。
    ///
    /// 返回影响行数（0 表示 part 下无可取消的活跃批次 —— 不视为错误，由 caller 决定）。
    pub async fn cancel_all_active_batches_for_part(
        conn: &mut PgConnection,
        part_id: i64,
        current_user_id: i64,
    ) -> Result<u64, AppError> {
        let out = status_gate::apply_bulk_batch_status_change_for_part(
            conn,
            status_gate::BulkStatusChange {
                part_id,
                new_status: "CANCELLED",
                excluded_statuses: &["COMPLETED", "CANCELLED"],
                // 终态批次不可能还在返修（review 第 1 轮 m10）
                is_repairing: Some(false),
                updated_by: current_user_id,
                derivation: status_gate::PartDerivation::KeepPartTerminalAsIs,
                // part 已是终态，派生层不会再写它 → 不需要归档事件 id
                event_id: None,
            },
        )
        .await?;
        Ok(out.affected_rows)
    }

    /// 2026-09-30 新增（force-complete 端点）：part 域 MANAGER 单角色强推工单 +
    /// 所有批次为 COMPLETED 的单 SQL 路径。
    ///
    /// 策略：**绕状态机** —— 所有非 CANCELLED、非软删的活跃批次一键推到
    /// COMPLETED（不走 OCC；version += 1；写 updated_by）。与
    /// `cancel_all_active_batches_for_part` 同形，但白名单排除 CANCELLED
    /// （终态不可被强推；CANCELLED 由 service 层守 `BIZ_PART_ALREADY_CANCELLED`）。
    ///
    /// 并发串行化由 SQL 行锁（part_id 索引 + 行锁）承担；无需 caller 侧
    /// `version` 校验（force-complete 是逃生通道，明确放弃 OCC 兜底）。
    ///
    /// 返回影响行数（0 表示 part 下无非 CANCELLED 活跃批次 —— 仍合法，由 caller
    /// 决定；如新建工单未拆批就是 0 行）。
    ///
    /// `event_id`（2026-10-01 review 第 1 轮 M4）：与 cancel 不同，本路径
    /// **必须**让 part 派生进 COMPLETED（escape hatch 的全部意义所在），故
    /// 终态序列号归档事件需要 caller 提供的雪花 id。
    ///
    /// `derivation = Rollup`（而非 cancel 那条路径的 `KeepPartTerminalAsIs`）：
    /// service 层已守「part 不得是 COMPLETED / CANCELLED」，故此处 part 一定
    /// 处于非终态，派生层可以正常写。
    pub async fn force_complete_all_batches_for_part(
        conn: &mut PgConnection,
        part_id: i64,
        current_user_id: i64,
        event_id: Option<i64>,
    ) -> Result<u64, AppError> {
        // 终态保护只守 CANCELLED（COMPLETED 幂等重写无副作用，与改造前一致）。
        let out = status_gate::apply_bulk_batch_status_change_for_part(
            conn,
            status_gate::BulkStatusChange {
                part_id,
                new_status: "COMPLETED",
                excluded_statuses: &["CANCELLED"],
                // 终态批次不可能还在返修（review 第 1 轮 m10）
                is_repairing: Some(false),
                updated_by: current_user_id,
                derivation: status_gate::PartDerivation::Rollup,
                event_id,
            },
        )
        .await?;
        Ok(out.affected_rows)
    }
}
