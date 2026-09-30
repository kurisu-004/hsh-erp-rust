//! part 域 SQL 真源 —— `t_part_batch` 表相关查询（2026-09-22 PR2 拆分原 sql.rs）
//!
//! ## 文件拆分原则（PR2 约定）
//! 按"主表归属"切分；函数体、签名、可见性、async 修饰全部保留；
//! **零 SQL 文本变化**（sqlx prepare 哈希一致）。
//!
//! ## 承载方法（19 个）
//!
//! ### find_*（7）
//! - `find_inprocess_batch_for_part` / `find_scan_target_batch`
//! - `find_inspection_batch_for_fail` / `find_current_inspection_batch_id`
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
//!
//! ### split（1）
//! - `split_batch_for_partial_pass`（薄包装 `PartBatchRepo::_split_batch_inner`）
//!
//! ## ZST `PartRepo`
//! ZST struct 在 `super`（sql/mod.rs）定义，本文件 `impl PartRepo { ... }`
//! 拼装。

use sqlx::{PgConnection, PgExecutor};

use super::PartRepo;
// PR2 合并后，`TPartBatch` 与 `PartBatchRepo` 都已搬到 `crate::modules::part::batch::*`。
// 本文件继续走新路径（与 PR2 「合并 part_batch → part/batch」约束一致）。
use crate::modules::part::batch::model::TPartBatch;

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
                       current_holder_id, current_process_id, current_process_step_id,
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
                               current_holder_id, current_process_id, current_process_step_id,
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
                       current_holder_id, current_process_id, current_process_step_id,
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
                               current_holder_id, current_process_id, current_process_step_id,
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
                       current_holder_id, current_process_id, current_process_step_id,
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
                               current_holder_id, current_process_id, current_process_step_id,
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
                   current_holder_id, current_process_id, current_process_step_id,
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
                -- 2026-09-30（review H2）：出池 → 池归属权威列置 NULL
                --   （不置 NULL 会让 INSPECTION 批次带着上一道工序 id 停留）
                current_process_id = NULL,
                -- 2026-09-16 PR-3：to_inspection 保留 current_process_step_id，
                --   但其定位已降级为「可选的显示用定位信息」（首次定位后不再推进）；
                --   to_process 会重新解析 step 写入，故此处不写不丢状态机上下文
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
    ///
    /// 2026-09-30 新增 `current_process_id: Option<i64>`：检验不合格打回生产架
    /// = **进池**，故写入目标工序（池归属权威依据）；`current_process_step_id`
    /// 仍是可选的显示用定位信息（首次定位后不再推进），允许 NULL。
    pub async fn mark_batch_failed_inspection<'e, E: PgExecutor<'e>>(
        executor: E,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_process_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET status                  = 'IN_PROCESS',
                location                = 'PRODUCTION_SHELF',
                current_holder_id       = $3,
                current_process_step_id = $4,
                current_process_id      = $6,
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
            current_process_id,
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
                   current_holder_id, current_process_id, current_process_step_id,
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
    /// （part/batch/repo.rs 的 `PartBatchRepo`）；`split_batch`（手动部分量）也
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
        crate::modules::part::batch::repo::PartBatchRepo::_split_batch_inner(
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
    pub async fn force_complete_all_batches_for_part<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            r#"
            UPDATE t_part_batch
            SET status     = 'COMPLETED',
                version    = version + 1,
                updated_at = now(),
                updated_by = $2
            WHERE part_id = $1 AND deleted_at IS NULL
              AND status <> 'CANCELLED'
            "#,
        )
        .bind(part_id)
        .bind(current_user_id)
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }
}
