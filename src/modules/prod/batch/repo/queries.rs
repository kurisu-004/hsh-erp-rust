//! `t_part_batch` SQL 真源 ZST `PartBatchRepo` + 固有静态方法。
//!
//! 2026-10-02 随 `t_part_batch` 归属迁入 prod 域（生产执行单元 = 批次）。
//! 本文件是 `t_part_batch` 的 **SQL 真源**，17 个 `PartBatchRepo::xxx` 静态方法；
//! 跨域调用方（part / assembly / delivery_note / wx / prod::queue / task）
//! 走 `PartBatchRepo::xxx(&mut *conn, ...)` 静态调用形态。
//!
//! ## 2026-10-08：`get_by_id` 与 `list_active_by_part_id` 已上移到 `shared::batch::read`
//!
//! 两者都是「批次这张表的公共读取单元」（跨域调用方：queue / delivery_note
//! / part / 派生链），留在本域等于让每个域反向 import prod::batch::repo。
//! `PartBatchRepoTrait` 的对应方法保留（trait 是本域对外的仓储契约），
//! 默认体改为转发 `shared::batch::read`。这不是转发壳 —— trait 方法是**接口**，
//! 实现换位置是正常演进。
//!
//! ## 本目录的 3 个文件
//! - `mod.rs` —— 本域 repo 子模块的声明与重导出
//! - `queries.rs`（本文件）—— `t_part_batch` 通用 SQL 真源，ZST `PartBatchRepo`
//! - `sql.rs` —— inspection / lifecycle 流转写点（`shared::batch::status` 之上的薄包装）
//! - `trait.rs` —— 胖 trait `PartBatchRepoTrait` + `impl for &mut PgConnection`
//!
//! 2026-10-07：原第 4 个文件 `list.rs`（待品检队列窄投影 + 返修集合读）连同其
//! 端点迁往 `prod::inspection`；返修两条集合读在 service 层自建 SQL，不在本目录。
//!
//! ## 错误类型
//! repo 静态方法 → `sqlx::Error`（唯一特殊：`split_batch` 内部守卫 0 行 →
//! `sqlx::Error::RowNotFound`，由 caller map）。

use chrono::NaiveDateTime;
use sqlx::{PgConnection, PgExecutor};

use crate::modules::part::model::TPart;
use crate::modules::prod::batch::model::{PartBatchScanRow, RecentBatchRow};
use crate::shared::batch::TPartBatch;
// 2026-10-01：`PartBatchRepo::update` 的 status 半边改走 shared::batch::status（唯一写入口）。
use crate::shared::batch::status as batch_status;
use crate::shared::batch::status::StatusChange;
use crate::shared::error::AppError;

/// SQL 真源 ZST。trait 名为 `PartBatchRepoTrait`（公共接口），
/// ZST 仍名 `PartBatchRepo`（跨模块静态调用方依赖此名）。
pub struct PartBatchRepo;

/// 初始批次 INSERT 入参（part/service/crud.rs 三创建入口共用）。
///
/// 2026-09-11 part/assembly/batch 重构方案 §4.1 (PR-B1)：新建 part 必须同事务
/// INSERT 一条 `batch_no=1` 的初始批次，否则车间流转（to_inspection / to_ship /
/// pickup）会因找不到 INSPECTION 批次而 20109 报错。
///
/// `location`：单件 / 批量 part 创建时为 `None`（part 行尚未确定 location）；
/// 子件创建（`insert_child_for_assembly`）时为 `Some("OFFICE")`（与子件 part
/// 行写入路径对齐）。
///
/// 2026-09-16 PR-3 批次 step 化（migration 028）：
/// - `next_process_id` / `placed_at` 字段删除（t_part_batch 列已删）
/// - 初始 batch 不在生产流内，`current_process_step_id = None`
pub struct NewInitialBatch<'a> {
    pub id: i64,
    pub part_id: i64,
    pub quantity: i32,
    pub location: Option<&'a str>,
    pub created_by: Option<i64>,
}

impl PartBatchRepo {
    /// 送货单的全部未删批次（Phase P2 列出 / Phase P3 扫码入单后回查）。
    /// 与 Python `list_by_delivery_note` 行为一致；本方法不 JOIN t_part，
    /// caller 需要展示字段时另调 `list_with_part_by_delivery_note`。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加
    /// current_process_step_id。
    pub async fn list_by_delivery_note<'e, E: PgExecutor<'e>>(
        executor: E,
        note_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, current_process_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   is_repairing,
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
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：JOIN 投影删 `pb.has_been_repaired`
    /// （t_part_batch 列已删）+ `p.actual_delivery_date` / `p.location` /
    /// `p.current_holder_id` / `p.placed_at` / `p.delivery_note_id` /
    /// `p.has_been_repaired`（t_part 列已删，6 个批次依附列）；TPart 字面量
    /// 回填同步。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：删 `pb.next_process_id` /
    /// `pb.placed_at`，加 `pb.current_process_step_id`。
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
                pb.current_process_id AS "pb_current_process_id?",
                pb.current_process_step_id AS "pb_current_process_step_id?",
                pb.delivery_note_id AS "pb_delivery_note_id?",
                pb.parent_batch_id AS "pb_parent_batch_id?",
                -- 2026-10-01 新增（migration 005）：返修标记随批次一同返回
                pb.is_repairing AS "pb_is_repairing!",
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
                p.applicant_name  AS "p_applicant_name!",
                p.quantity        AS "p_quantity!",
                p.request_date    AS "p_request_date!",
                p.planned_delivery_date AS "p_planned_delivery_date!",
                p.is_urgent       AS "p_is_urgent!",
                p.next_process_id   AS "p_next_process_id?",
                p.order_no        AS "p_order_no?",
                p.system_delivery_date AS "p_system_delivery_date?",
                p.note            AS "p_note?",
                p.unit_price      AS "p_unit_price!",
                p.total_price     AS "p_total_price!",
                p.process_chain_id AS "p_process_chain_id?"
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
                        current_process_id: r.pb_current_process_id,
                        current_process_step_id: r.pb_current_process_step_id,
                        delivery_note_id: r.pb_delivery_note_id,
                        parent_batch_id: r.pb_parent_batch_id,
                        is_repairing: r.pb_is_repairing,
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
                        customer_id: r.p_customer_id,
                        assembly_id: r.p_assembly_id,
                        status: r.p_status,
                        is_urgent: r.p_is_urgent,
                        next_process_id: r.p_next_process_id,
                        order_no: r.p_order_no,
                        system_delivery_date: r.p_system_delivery_date,
                        note: r.p_note,
                        unit_price: r.p_unit_price,
                        total_price: r.p_total_price,
                        version: r.p_version,
                        created_at: r.p_created_at,
                        created_by: r.p_created_by,
                        updated_at: r.p_updated_at,
                        updated_by: r.p_updated_by,
                        deleted_at: r.p_deleted_at,
                        process_chain_id: r.p_process_chain_id,
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
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：JOIN 投影同步删 `pb.has_been_repaired`
    /// + `p.actual_delivery_date` / `p.location` / `p.current_holder_id` /
    ///   `p.placed_at` / `p.delivery_note_id` / `p.has_been_repaired` 6 列；
    ///   TPart 字面量回填同步。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：删 `pb.next_process_id` /
    /// `pb.placed_at`，加 `pb.current_process_step_id`。
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
                pb.current_process_id AS "pb_current_process_id?",
                pb.current_process_step_id AS "pb_current_process_step_id?",
                pb.delivery_note_id AS "pb_delivery_note_id?",
                pb.parent_batch_id AS "pb_parent_batch_id?",
                -- 2026-10-01 新增（migration 005）：返修标记随批次一同返回
                pb.is_repairing AS "pb_is_repairing!",
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
                p.applicant_name  AS "p_applicant_name!",
                p.quantity        AS "p_quantity!",
                p.request_date    AS "p_request_date!",
                p.planned_delivery_date AS "p_planned_delivery_date!",
                p.is_urgent       AS "p_is_urgent!",
                p.next_process_id   AS "p_next_process_id?",
                p.order_no        AS "p_order_no?",
                p.system_delivery_date AS "p_system_delivery_date?",
                p.note            AS "p_note?",
                p.unit_price      AS "p_unit_price!",
                p.total_price     AS "p_total_price!",
                p.process_chain_id AS "p_process_chain_id?"
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
                        current_process_id: r.pb_current_process_id,
                        current_process_step_id: r.pb_current_process_step_id,
                        delivery_note_id: r.pb_delivery_note_id,
                        parent_batch_id: r.pb_parent_batch_id,
                        is_repairing: r.pb_is_repairing,
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
                        customer_id: r.p_customer_id,
                        assembly_id: r.p_assembly_id,
                        status: r.p_status,
                        is_urgent: r.p_is_urgent,
                        next_process_id: r.p_next_process_id,
                        order_no: r.p_order_no,
                        system_delivery_date: r.p_system_delivery_date,
                        note: r.p_note,
                        unit_price: r.p_unit_price,
                        total_price: r.p_total_price,
                        version: r.p_version,
                        created_at: r.p_created_at,
                        created_by: r.p_created_by,
                        updated_at: r.p_updated_at,
                        updated_by: r.p_updated_by,
                        deleted_at: r.p_deleted_at,
                        process_chain_id: r.p_process_chain_id,
                    },
                )
            })
            .collect())
    }

    /// 多工单的未删批次批查（Phase P3 装配件整套入单时按子件 part_ids 一次拿齐）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加
    /// current_process_step_id。
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
                   current_holder_id, current_process_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   is_repairing,
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
    ///
    /// ## 2026-10-01：`status` 那一半改走 shared::batch::status
    ///
    /// 改造前本函数是「一处 UPDATE 同时写 `delivery_note_id` + `status`」，
    /// 于是它本身就是一个**绕过 rollup 的状态写点**（`status` 列在
    /// `t_part_batch` 上，是全链路真源）。现拆成两步：
    ///
    /// 1. `status` 非 None → [`batch_status::apply_batch_status_change`]
    ///    （写状态 + part/assembly 派生 + 终态序列号释放一体）。
    /// 2. `delivery_note_id` 非 None → 独立的「只挂送货单」UPDATE。
    ///
    /// **为什么第 2 步不 bump version**：第 1 步已经在**同一事务、同一行**
    /// 上做过一次带 `WHERE version = $n` 的 OCC 校验并持有了该行的行锁，
    /// 在 READ COMMITTED 下并发写者会阻塞到本事务结束 —— 第 2 步再做一次
    /// OCC 校验并不能再拦住任何冲突（只会误伤：第 1 步刚把 version 加了 1），
    /// 只会为**一次业务动作**把审计计数器 `version` 加 2，让
    /// `t_part_batch.version` 增长速度与业务动作数脱钩。故第 2 步只写列、
    /// 不动 `version` / `updated_at` / `updated_by`（审计时间戳由第 1 步给出，
    /// 语义上更准确：业务动作发生在那一刻）。
    ///
    /// ## 返回值契约（2026-10-01 review 第 1 轮 M6 修正）
    ///
    /// 返回**真实影响行数**，而不是恒 1：
    /// - `status = Some(..)` → 走 shared::batch::status，0 行已由 gate 抛 `VERSION_CONFLICT`，
    ///   走到这里必然是 1；第 2 步是「值没变就不写」，可能 0 行，但**不**把它
    ///   计入返回值（那会让唯一生产调用方 `delivery_note::pickup` 在「送货单 id
    ///   本来就一样」时误报并发冲突）。
    /// - `status = None` → 只有第 2 步，返回它的真实行数；此时第 2 步**带 OCC**
    ///   （`AND version = $3`），故 0 行确实意味着版本冲突，caller 转 409 是对的。
    ///
    /// 上一版恒返回 `Ok(1)` 且在 `status = None` 时完全没有 OCC（`version` 形参
    /// 被忽略），使 trait 声明里「0 行 → 由 service 转 `VERSION_CONFLICT` 409」
    /// 变成一句谎言、`delivery_note/service/lifecycle.rs` 的 `if affected == 0`
    /// 变成死代码。
    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        conn: &mut PgConnection,
        batch_id: i64,
        version: i32,
        delivery_note_id: Option<i64>,
        status: Option<&str>,
        updated_by: Option<i64>,
    ) -> Result<u64, AppError> {
        let updated_by = updated_by.unwrap_or(0);
        // 1) status → shared::batch::status（唯一写入口；0 行 = VERSION_CONFLICT）
        if let Some(target) = status {
            batch_status::apply_batch_status_change(
                conn,
                StatusChange {
                    batch_id,
                    new_status: target,
                    new_location: None,
                    new_holder_id: None,
                    new_process_id: None,
                    new_process_step_id: None,
                    is_repairing: None,
                    expected_version: Some(version),
                    // 唯一生产调用方是 `delivery_note::pickup`，源恒为
                    // READY_TO_SHIP（service 层逐批守过）。
                    allowed_from: &["READY_TO_SHIP"],
                    updated_by,
                    clear_location: false,
                    clear_holder_id: false,
                    clear_process_id: false,
                    clear_process_step_id: false,
                    // 目标状态 DELIVERED 不是终态 → 终态归档事件分支不可达（M4）
                    event_id: None,
                },
            )
            .await?;
            // 2) delivery_note_id → 只挂单，不动 version（理由见上方 doc）
            if let Some(note_id) = delivery_note_id {
                sqlx::query(
                    "UPDATE t_part_batch SET delivery_note_id = $2 \
                     WHERE id = $1 AND deleted_at IS NULL \
                       AND delivery_note_id IS DISTINCT FROM $2::bigint",
                )
                .bind(batch_id)
                .bind(note_id)
                .execute(&mut *conn)
                .await?;
            }
            return Ok(1);
        }
        // 仅挂送货单（`status = None`）：带 OCC，0 行 → version 冲突。
        // 语义上等价于 `attach_to_note`，但保留本函数是为了不破坏
        // 「一次调用同时给两列赋值」的既有调用点签名。
        let mut affected = 0u64;
        if let Some(note_id) = delivery_note_id {
            let r = sqlx::query(
                "UPDATE t_part_batch SET delivery_note_id = $2 \
                 WHERE id = $1 AND version = $3 AND deleted_at IS NULL \
                   AND delivery_note_id IS DISTINCT FROM $2::bigint",
            )
            .bind(batch_id)
            .bind(note_id)
            .bind(version)
            .execute(&mut *conn)
            .await?;
            affected = r.rows_affected();
        }
        Ok(affected)
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

    /// 拆分批次（PR-4 卫生项 B2）：`_split_batch_inner` 是公共实现，
    /// `split_batch`（手动部分量）+ `PartRepo::split_batch_for_partial_pass`
    /// （to_ship/to_process/to_inspection 部分通过）都委托到这里。
    ///
    /// 行为：
    /// 1. 同 part_id 下 max(batch_no) + 1（与 uq_t_part_batch_part_no 对齐）
    /// 2. `INSERT ... SELECT FROM t_part_batch WHERE id = source_id`：
    ///    - 继承源 location / current_holder_id / current_process_step_id
    ///    - **不**继承 delivery_note_id（拆出批次独立流转）
    ///    - 写 parent_batch_id = source_batch_id
    ///    - quantity = caller 传入的 qty
    ///    - status = caller 传入的新批次 status（通常等于源 status）
    /// 3. 源批次 quantity -= qty（OCC + 数量守卫；0 行 → conflict）
    ///
    /// 返回新批次雪花 id（caller 拿到后做后续 attach_to_note 等操作）。
    ///
    /// 注：本函数需在同一事务内连发三条 SQL（max + insert + update），而
    /// `impl PgExecutor<'_>` 不能 move 多次，因此显式收 `&mut PgConnection`。
    ///
    /// 2026-09-17 PR-4 卫生项 B2 合并：把 `split_batch`（part_batch/repo.rs）
    /// 与 `split_batch_for_partial_pass`（part/repo/batch.rs）80% 重叠的逻辑
    /// 抽到这里；两个公开 fn 改为薄包装。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：删 `next_process_id` /
    /// `placed_at` 列写入；`current_process_step_id` 走 SELECT 继承源批次。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn _split_batch_inner(
        conn: &mut PgConnection,
        new_batch_id: i64,
        source_batch_id: i64,
        source_version: i32,
        part_id: i64,
        qty: i32,
        new_batch_status: &str,
        user_id: i64,
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

        // 2. INSERT 新批次（继承源 location / current_holder_id /
        //    current_process_step_id / **current_process_id**（2026-09-30 池归属
        //    权威依据，拆批不改变批次所属工序）/ **is_repairing**（2026-10-01
        //    migration 005：返修是**当前态**，拆批切走的是「同一批实物里的一部分」，
        //    其返修上下文与源批次同源，故随源继承，避免拆批把返修中批次悄悄
        //    洗成非返修）；quantity = qty；
        //    status = new_batch_status；不继承 delivery_note_id；写 parent_batch_id）。
        sqlx::query!(
            r#"
            INSERT INTO t_part_batch (
                id, part_id, batch_no, quantity, status, location,
                current_holder_id, current_process_id, current_process_step_id,
                delivery_note_id, parent_batch_id,
                is_repairing,
                version, created_at, created_by, updated_at, updated_by
            )
            SELECT $1, part_id, $2, $3, $4, location,
                   current_holder_id, current_process_id, current_process_step_id,
                   NULL, $5,
                   is_repairing,
                   0, now(), $6, now(), $6
            FROM t_part_batch
            WHERE id = $7 AND deleted_at IS NULL
            "#,
            new_batch_id,
            next_batch_no,
            qty,
            new_batch_status,
            source_batch_id,
            user_id,
            source_batch_id,
        )
        .execute(&mut *conn)
        .await?;

        // 3. 源批次 quantity -= qty（OCC + 数量守卫；0 行 → conflict）。
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
            source_batch_id,
            source_version,
            qty,
            user_id,
        )
        .execute(&mut *conn)
        .await?;
        if res.rows_affected() == 0 {
            return Err(sqlx::Error::RowNotFound);
        }

        Ok(new_batch_id)
    }

    /// 拆分批次（手动部分量）：`POST /prod/batches/{batch_id}/split` 入口。
    ///
    /// 在 `qty` < `source_batch.quantity` 时调用，构造一条新批次（继承
    /// 状态/位置/holder/current_process_step；**不继承** delivery_note_id 与
    /// parent_batch_id——这里 parent_batch_id = source_batch_id 写入以保留
    /// 拆分谱系），并把源批次 quantity 减 `qty`。整组写在一个 tx 内。
    ///
    /// 返回新批次雪花 id（caller 拿到后做后续 attach_to_note）。`batch_no` 用
    /// 「当前 part_id 下 max(batch_no) + 1」生成。
    ///
    /// 镜像 Python `service/_batch_ops::split_batch`。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：
    /// - `next_process_id` / `placed_at` 参数删除（t_part_batch 列已删）
    /// - `current_process_step_id` 由内部 `_split_batch_inner` 走 SELECT 继承源
    /// - 2026-09-30：`current_process_id`（池归属权威依据）同样由 SELECT 继承
    ///
    /// 2026-09-17 PR-4 卫生项 B2：薄包装委托到 `_split_batch_inner`。
    #[allow(clippy::too_many_arguments)]
    pub async fn split_batch(
        conn: &mut PgConnection,
        new_batch_id: i64,
        source_batch_id: i64,
        source_version: i32,
        part_id: i64,
        qty: i32,
        status: &str,
        location: Option<&str>,
        current_holder_id: Option<i64>,
        current_process_step_id: Option<i64>,
        when: chrono::NaiveDateTime,
        created_by: Option<i64>,
        updated_by: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        // location / current_holder_id / current_process_step_id 参数保留以
        // 兼容 `PartService::split_batch` 调用方（caller 已知源当前值，传冗余
        // 但不影响最终 INSERT——`_split_batch_inner` 走 SELECT 重新读源行做真相
        // 源）。when / created_by / updated_by 也保留以兼容 service 拼装
        // 现有调用形态；内部 helper 改用 now() + caller 的 user_id。
        let _ = (
            location,
            current_holder_id,
            current_process_step_id,
            when,
            created_by,
        );
        let user_id = updated_by.unwrap_or(0);
        Self::_split_batch_inner(
            conn,
            new_batch_id,
            source_batch_id,
            source_version,
            part_id,
            qty,
            status,
            user_id,
        )
        .await
    }

    /// 工单全部活跃批次 + 批次自身状态（rollup 用）。
    /// 不传 `include_deleted`：rollup 只看活跃行。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加
    /// Scan context 专用：返回工单全部活跃批次 + 持有人/货架**名称**（已解析）。
    /// `holder_name` 通过 `COALESCE(t_shelf.name, t_worker.name, t_outsource_company.name)` 解析，
    /// 适用于 `current_holder_id` 多态（shelf / worker / outsource 的 holder_id）。
    /// 按 `batch_no ASC` 排序，frontend 可直接渲染顺序。
    pub async fn list_active_by_part_id_with_holder<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<Vec<PartBatchScanRow>, sqlx::Error> {
        sqlx::query!(
            r#"
            SELECT
                pb.id       AS "id!",
                pb.quantity AS "quantity!",
                pb.status   AS "status!",
                pb.version  AS "version!",
                COALESCE(s.name, w.name, oc.name) AS "holder_name?"
            FROM t_part_batch pb
            LEFT JOIN t_shelf            s  ON s  .id = pb.current_holder_id
            LEFT JOIN t_worker           w  ON w  .id = pb.current_holder_id
            LEFT JOIN t_outsource_company oc ON oc.id = pb.current_holder_id
            WHERE pb.part_id = $1 AND pb.deleted_at IS NULL
            ORDER BY pb.batch_no ASC
            "#,
            part_id,
        )
        .fetch_all(executor)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|row| PartBatchScanRow {
                    id: row.id,
                    quantity: row.quantity,
                    status: row.status,
                    holder_name: row.holder_name,
                    version: row.version,
                })
                .collect()
        })
    }

    /// 多 part 全部活跃批次批查（pickup rollup 用）。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加
    /// current_process_step_id。
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
                   current_holder_id, current_process_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   is_repairing,
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
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：JOIN 投影删 `pb.has_been_repaired`
    /// + 6 个 t_part 批次依附列；TPart 字面量回填同步。
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加
    /// current_process_step_id。
    pub async fn list_batches_with_part_in_customers<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        customer_ids: &[i64],
        limit: i64,
    ) -> Result<Vec<(TPartBatch, TPart)>, sqlx::Error> {
        if customer_ids.is_empty() || statuses.is_empty() {
            return Ok(Vec::new());
        }
        let sql = r#"
            SELECT
                pb.id, pb.part_id, pb.batch_no, pb.quantity, pb.status, pb.location,
                pb.current_holder_id, pb.current_process_id, pb.current_process_step_id,
                pb.delivery_note_id, pb.parent_batch_id,
                -- 2026-10-01 新增（migration 005）
                pb.is_repairing,
                pb.version, pb.created_at, pb.created_by, pb.updated_at, pb.updated_by, pb.deleted_at,
                p.id AS "p_id", p.serial_no AS "p_serial_no", p.name AS "p_name",
                p.drawing_no AS "p_drawing_no", p.customer_id AS "p_customer_id",
                p.assembly_id AS "p_assembly_id", p.status AS "p_status",
                p.version AS "p_version", p.created_at AS "p_created_at",
                p.created_by AS "p_created_by", p.updated_at AS "p_updated_at",
                p.updated_by AS "p_updated_by", p.deleted_at AS "p_deleted_at",
                p.applicant_name AS "p_applicant_name",
                p.quantity AS "p_quantity",
                p.request_date AS "p_request_date",
                p.planned_delivery_date AS "p_planned_delivery_date",
                p.is_urgent AS "p_is_urgent",
                p.next_process_id AS "p_next_process_id",
                p.order_no AS "p_order_no",
                p.system_delivery_date AS "p_system_delivery_date",
                p.note AS "p_note",
                p.unit_price AS "p_unit_price",
                p.total_price AS "p_total_price",
                p.process_chain_id AS "p_process_chain_id"
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
                current_process_id: r.try_get("current_process_id")?,
                current_process_step_id: r.try_get("current_process_step_id")?,
                delivery_note_id: r.try_get("delivery_note_id")?,
                parent_batch_id: r.try_get("parent_batch_id")?,
                is_repairing: r.try_get("is_repairing")?,
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
                customer_id: r.try_get("p_customer_id")?,
                assembly_id: r.try_get("p_assembly_id")?,
                status: r.try_get("p_status")?,
                is_urgent: r.try_get("p_is_urgent")?,
                next_process_id: r.try_get("p_next_process_id")?,
                order_no: r.try_get("p_order_no")?,
                system_delivery_date: r.try_get("p_system_delivery_date")?,
                note: r.try_get("p_note")?,
                unit_price: r.try_get("p_unit_price")?,
                total_price: r.try_get("p_total_price")?,
                version: r.try_get("p_version")?,
                created_at: r.try_get("p_created_at")?,
                created_by: r.try_get("p_created_by")?,
                updated_at: r.try_get("p_updated_at")?,
                updated_by: r.try_get("p_updated_by")?,
                deleted_at: r.try_get("p_deleted_at")?,
                process_chain_id: r.try_get("p_process_chain_id")?,
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
    ///
    /// 2026-09-16 PR-3 批次 step 化：删 next_process_id / placed_at，加
    /// current_process_step_id。
    pub async fn list_held_by_worker<'e, E: PgExecutor<'e>>(
        executor: E,
        worker_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT id, part_id, batch_no, quantity, status, location,
                   current_holder_id, current_process_id, current_process_step_id,
                   delivery_note_id, parent_batch_id,
                   is_repairing,
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

    /// DELIVERED 状态且 DELIVERED 事件 `created_at` 早于 threshold 的批次 ID 列表。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：
    /// - 原口径 `placed_at < threshold` 已废弃：`placed_at` 列被删（不再统计生产时间）
    /// - 新口径：读 `t_part_event` 中 `event_type='DELIVERED'` 事件的 `created_at`
    ///   作为真实 DELIVERED 时间戳（与 Python `_run_once` 的 latest_event 派生
    ///   口径对齐，避免 `placed_at`（首次 ON_SHELF 时间）与 DELIVERED 时间偏差）
    pub async fn find_delivered_older_than<'e, E: PgExecutor<'e>>(
        executor: E,
        threshold: NaiveDateTime,
    ) -> Result<Vec<(i64, i64, i32)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"
            SELECT b.id        AS "id!",
                   b.part_id   AS "part_id!",
                   b.version   AS "version!"
            FROM t_part_batch b
            WHERE b.status     = 'DELIVERED'
              AND b.deleted_at IS NULL
              AND EXISTS (
                  SELECT 1 FROM t_part_event e
                  WHERE e.batch_id = b.id
                    AND e.event_type = 'DELIVERED'
                    AND e.created_at < $1
              )
            ORDER BY b.id ASC
            "#,
            threshold,
        )
        .fetch_all(executor)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.id, r.part_id, r.version))
            .collect())
    }

    /// 创建初始批次（part/assembly/batch 重构方案 §4.1 PR-B1）。
    ///
    /// 在 part 创建入口（`create_part` / `batch_create_parts` / `insert_child_for_assembly`）
    /// 同事务内调用，插入 `batch_no=1 / status='PENDING' / version=0 /
    /// current_holder_id=NULL / current_process_step_id=NULL /
    /// delivery_note_id=NULL / parent_batch_id=NULL` 的初始批次。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：`has_been_repaired` 列已删，INSERT
    /// 同步删该列；返修事实由 `t_part_event` REPAIR_STARTED 事件追溯。
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：
    /// - 删 `next_process_id` / `placed_at` 列写入
    /// - `current_process_step_id = NULL`（初始 batch 不在生产流）
    ///
    /// `location` 单件 / 批量创建时传 `None`，子件创建时传 `Some("OFFICE")`（与
    /// `insert_child_for_assembly` 写入子件 part 行时的 location 对齐）。
    ///
    /// 重复插入（已存在 `batch_no=1` 的活跃批次）由 `uq_t_part_batch_part_no`
    /// 触发 23505，由 caller 决定是否 swallow；正常创建入口不会触发。
    ///
    /// 返回写入行 id（与 `new.id` 一致；显式 RETURNING 兼容未来 trigger）。
    pub async fn create_initial_batch<'e, E: PgExecutor<'e>>(
        executor: E,
        new: NewInitialBatch<'_>,
    ) -> Result<i64, sqlx::Error> {
        let id: i64 = sqlx::query_scalar!(
            r#"
            INSERT INTO t_part_batch (
                id, part_id, batch_no, quantity, status, location,
                current_holder_id, current_process_step_id,
                delivery_note_id, parent_batch_id,
                version, created_at, created_by, updated_at, updated_by
            ) VALUES (
                $1, $2, 1, $3, 'PENDING', $4,
                NULL, NULL,
                NULL, NULL,
                0, now(), $5, now(), $5
            )
            RETURNING id AS "id!"
            "#,
            new.id,
            new.part_id,
            new.quantity,
            new.location,
            new.created_by,
        )
        .fetch_one(executor)
        .await?;
        Ok(id)
    }

    /// 2026-09-16 PR-2 瘦身（migration 027）：替代 `t_part.delivery_note_id`
    /// 守卫（Finding D）。part 是否「已挂送货单」改查其任意活跃批次的
    /// `delivery_note_id IS NOT NULL`（真相源在 t_part_batch）。
    ///
    /// 用于：
    /// - `PartService::cancel` 守 21420 `BIZ_DELIVERY_NOTE_LOCKED_PART`
    /// - `PartService::soft_delete_part` 预检（替代原 `soft_delete_part` UPDATE
    ///   内 `delivery_note_id IS NULL` 条件）
    /// - `AssemblyService::soft_delete_assembly` 子件预检
    ///
    /// 返回 `true` ⇔ part 至少有 1 条 `deleted_at IS NULL` 活跃批次的
    /// `delivery_note_id` 非空。0 行 ⇒ `false`（无活跃批次 / 全无挂单）。
    pub async fn has_active_batch_on_delivery_note<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
    ) -> Result<bool, sqlx::Error> {
        let row: Option<(i64,)> = sqlx::query_as(
            r#"
            SELECT 1::bigint
            FROM t_part_batch
            WHERE part_id = $1
              AND delivery_note_id IS NOT NULL
              AND deleted_at IS NULL
            LIMIT 1
            "#,
        )
        .bind(part_id)
        .fetch_optional(executor)
        .await?;
        Ok(row.is_some())
    }
}
