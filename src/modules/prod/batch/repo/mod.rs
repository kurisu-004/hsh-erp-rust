//! `t_part_batch` repo 层 —— SQL 真源
//!
//! 2026-10-02 域迁移：`t_part_batch` 是生产执行单元，本表的 repo 层整体归
//! `prod::batch`。原 `part/batch/repo.rs`（单文件 1785 行）拆为
//! `queries.rs` / `sql.rs` / `list.rs` / `trait.rs` 四文件；原
//! `part/repo/sql/batch_sql.rs`（`impl PartRepo` 的 19 个流转写点）并入
//! `sql.rs`，impl 目标由 `PartRepo` 改为本域 ZST `PartBatchRepo`。
//!
//! ## 两个 ZST 的分工（命名相近，注意区分）
//! - `PartBatchRepo` —— `t_part_batch` 的**通用** SQL 真源，全仓读写批次表的
//!   默认入口：18 个通用方法（`queries.rs`）+ 19 个流转写点（`sql.rs`）+
//!   2 个集合读（`list.rs`：3-JOIN 窄投影 + 表头筛选/排序）。跨域调用方
//!   一律走它。
//! - `BatchRepo`（本文件）—— **只服务「PENDING 批次下发给车间」一条流**的专用
//!   查询（7 个方法），唯一调用方是 `service::dispatch.rs`。
//!   两者都是无状态 ZST + 固有静态方法，职责不重叠。
//!
//! ## 文件分工
//! - `queries.rs` —— ZST `PartBatchRepo` + 18 个通用静态方法
//!   （`create_initial_batch` / `get_by_id` / `update` / `attach_to_note` /
//!   `split_batch` / 工人持有件查询 等）
//! - `sql.rs` —— inspection / lifecycle 流转的 19 个定位 + 写点
//!   （`find_*` / `mark_*` / `split_batch_for_partial_pass` /
//!   `cancel_all_active_batches_for_part` / `force_complete_all_batches_for_part`）
//! - `list.rs` —— 集合读 2 条：`list_inspection_queue` / `count_inspection_queue`
//!   （3-JOIN 窄投影 + 表头筛选/排序，服务 `GET /prod/batches/inspection`；
//!   list 与 count 共用同一个 WHERE 拼装器，判据只此一份）
//! - `trait.rs` —— 胖 trait `PartBatchRepoTrait` + `impl for &mut PgConnection`
//! - `mod.rs`（本文件）—— ZST `BatchRepo`：本域「PENDING 批次下发给车间」
//!   专用查询（pending 列表 / auto-dispatch 预览 / 首道 step / 兜底反查 part_id）
//!
//! ## 2026-10-02 去重
//! 本文件原 `BatchRepo::find_batch_by_id`（`WHERE id = $1 AND ($2 OR deleted_at IS NULL)`）
//! 与 `queries.rs::PartBatchRepo::get_by_id` 是同一条 SQL 的两份实现，已删，
//! 调用方（`service::dispatch_single`）改调 `PartBatchRepo::get_by_id`。
//!
//! ## 已知缺陷：holder 三表 COALESCE 的多态歧义
//! `holder_name` 一律按 `COALESCE(s.name, w.name, oc.name)` 解析
//! （`t_shelf` / `t_worker` / `t_outsource_company` 三表对同一个
//! `current_holder_id` 各 JOIN 一次）。该写法**假定** holder id 在三表 PK 空间里
//! 互不重叠；一旦某 id 同时命中其中两表，取到的是 `t_shelf.name`。
//!
//! 全仓 holder 名多表 COALESCE 共 **6 处**（4 处 `t_shelf.name` 形态 + 2 处
//! `t_shelf.code` 变体，后者歧义时取 `t_shelf.code` 而非 name）：
//!
//! - `prod/batch/repo/queries.rs::PartBatchRepo::list_active_by_part_id_with_holder`
//! - `prod/batch/service/repair.rs::list_batches_matching`（`sqlx::query_as` 内联 SQL）
//! - `part/service/phase1/lifecycle_helpers.rs::list_batches`
//! - `prod/inspection/repo.rs::InspectionScanRepo::list_batches_by_part_ids`
//!   （2026-10-05 新增的扫码树批次层）
//! - `wx/repo.rs::PartList::list`
//! - `wx/repo.rs::PartList::by_serial`
//!
//! dashboard 域**不在此列** —— 它的 SQL 只投影 `b.current_holder_id AS holder_id`，
//! 名字在 service 侧按各自 JOIN 来源装配（货架分组自带 `shelf_name`，工人分组查
//! `worker_name_map`，见 `dashboard/service/snapshot.rs`），不在 SQL 里做多表 COALESCE。
//!
//! 正确解法是用 `t_part_batch.location` 作 discriminator
//! （`CASE location WHEN 'WORKER' THEN w.name WHEN 'OUTSOURCE_COMPANY' THEN oc.name
//! ELSE s.name END`）。**本次不动**：改这一处会让 6 条 SQL 的行为对某些历史脏数据
//! 发生变化，属独立改动，且当务之急是修「返修列表 holder 名解析错」还是「先补
//! 脏数据清洗」需要产品侧确认。修时必须 6 处一起改，逐处改会造成同一 holder 在
//! 不同端点显示不同名字。
//!
//! ## 错误类型
//! repo 静态方法 → `sqlx::Error`（与项目惯例一致），由 service 层映射 `AppError`。

pub mod list;
pub mod queries;
pub mod sql;
pub mod r#trait;

pub use queries::{NewInitialBatch, PartBatchRepo};
pub use r#trait::PartBatchRepoTrait;

use chrono::NaiveDate;
use sqlx::PgConnection;

use crate::shared::error::AppError;

/// `prod::batch` ZST 静态方法容器。
pub struct BatchRepo;

impl BatchRepo {
    /// 待下发批次列表（JOIN 4 表）。
    ///
    /// 2026-10-02：本方法只服务「下发车间」一条流（`list_pending` / `auto_dispatch`），
    /// 其投影比通用读 `queries::list_batches_with_part_in_customers` 宽：额外
    /// LEFT JOIN `t_part.process_chain_id` 与 `pb.current_process_step_id`（批次
    /// step 化字段，下发时要定位首道工序）。两者投影不同，**不是**同一 SQL 的两份
    /// 实现，不做合并。
    ///
    /// 2026-10-06：状态闸门由 `pb.status = 'PENDING'` 放宽为
    /// `pb.status IN ('PENDING', 'PROGRAMMING')`。`PROGRAMMING` 是**已废弃**状态
    /// （`part::statemachine` 中 `PENDING → PROGRAMMING` 的入口端点已下线，只保留
    /// 4 条出口），但存量行仍需在「待下发」页被消化掉，故与 `PENDING` **同链路、
    /// 同待遇**：同样出现在本列表、同样能被 `dispatch_batch` / `auto_dispatch`
    /// 下发。白名单与 `count_pending_batches` / `preview_auto_dispatch` /
    /// `update_batch_dispatched` 的 `allowed_from` 必须四处同步，否则「列得出
    /// 但下发不了」或 `total` 与 `items` 口径不一致。
    ///
    /// 排序：`p.system_delivery_date ASC NULLS LAST, p.is_urgent DESC,
    /// pb.created_at ASC`（计划交期近 + 加急件优先 + 批次入库时间兜底）。
    ///
    /// 默认 `limit=200, offset=0`（由 handler 层 `ListPendingQuery` 默认值兜底）。
    ///
    /// 返回 `Vec<PendingBatchRow>` —— service 内转换为 `vo::PendingBatchItem`。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_pending_batches(
        conn: &mut PgConnection,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PendingBatchRow>, sqlx::Error> {
        let rows = sqlx::query_as!(
            PendingBatchRow,
            r#"
            SELECT
                pb.id              AS "pb_id!",
                pb.part_id         AS "pb_part_id!",
                pb.batch_no        AS "pb_batch_no!",
                pb.quantity        AS "pb_quantity!",
                pb.status          AS "pb_status!",
                pb.version         AS "pb_version!",
                pb.current_process_step_id AS "pb_current_process_step_id?",
                pb.created_at      AS "pb_created_at!",
                p.serial_no        AS "p_serial_no?",
                p.name             AS "p_name!",
                p.drawing_no       AS "p_drawing_no!",
                p.planned_delivery_date AS "p_planned_delivery_date!",
                p.system_delivery_date  AS "p_system_delivery_date?",
                p.is_urgent        AS "p_is_urgent!",
                p.note             AS "p_note?",
                p.process_chain_id AS "p_process_chain_id?",
                c.name             AS "c_name?",
                pc.name            AS "pc_name?",
                a.name             AS "a_name?"
            FROM t_part_batch pb
            JOIN t_part p
              ON p.id = pb.part_id
            LEFT JOIN t_customer c
              ON c.id = p.customer_id
            LEFT JOIN t_customer pc
              ON pc.id = c.parent_id
            LEFT JOIN t_applicant a
              ON a.name = p.applicant_name AND a.deleted_at IS NULL
            WHERE pb.status IN ('PENDING', 'PROGRAMMING')
              AND pb.deleted_at IS NULL
              AND p.deleted_at IS NULL
            ORDER BY
                p.system_delivery_date ASC NULLS LAST,
                p.is_urgent DESC,
                pb.created_at ASC,
                pb.id ASC
            LIMIT $1 OFFSET $2
            "#,
            limit,
            offset,
        )
        .fetch_all(&mut *conn)
        .await?;

        Ok(rows)
    }

    /// 待下发列表配套 COUNT（与 `list_pending_batches` 同 WHERE 不同 SELECT）。
    ///
    /// 2026-10-06：状态闸门必须与 `list_pending_batches` 逐字同步
    /// （`IN ('PENDING', 'PROGRAMMING')`），否则 `total` 与 `items` 对不上。
    pub async fn count_pending_batches(conn: &mut PgConnection) -> Result<i64, sqlx::Error> {
        let n: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch pb
            JOIN t_part p
              ON p.id = pb.part_id
            WHERE pb.status IN ('PENDING', 'PROGRAMMING')
              AND pb.deleted_at IS NULL
              AND p.deleted_at IS NULL
            "#,
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(n)
    }

    /// 标记待下发批次（`PENDING` / `PROGRAMMING`）已下发（OCC UPDATE）。
    ///
    /// 输入：batch_id, expected_version (批次当前 version), shelf_id,
    /// updated_by, current_process_id（= target_process_id）。
    /// 输出：affected rows（0 → 40901 `VERSION_CONFLICT` / status 不在
    /// `allowed_from` 白名单 / 已软删，由 service 层映射）。
    ///
    /// 副作用：`status='IN_PROCESS'` + `location='PRODUCTION_SHELF'` +
    /// `current_holder_id=shelf_id` + `current_process_id=target_process_id` +
    /// `version += 1`。
    ///
    /// 2026-09-30 新增 `current_process_id` 写入（用户报告 bug 修复）：本列是
    /// **判断批次是否属于某工序池的唯一权威依据**（worker_pool 候选池 3 条 SQL
    /// 与 count 全部按它普通过滤）。此前 dispatch 只写
    /// `current_process_step_id=NULL`，而候选池 SQL 全部 INNER JOIN
    /// `t_process_chain_step ON s.id = pb.current_process_step_id`：
    /// `s.id = NULL` 匹配不到任何行，批次对所有池查询隐身（前端看到
    /// 「下发成功但工序池里没有」），且因唯一推进 step 的 worker-scan 路径又
    /// 要求批次先在池里，形成死状态。
    ///
    /// `current_process_step_id` 仍写 NULL 是**有意的**：本次不解析 step
    /// （工单无工序链时本就解析不出）。该列已降级为**可选的显示用定位信息**，
    /// NULL 不影响入池。
    ///
    /// `current_process_step_id` 的**已知缺口**：本列只在其它「首次定位工序」
    /// 的写点被写入（place_on_shelf / release_from_programming / outsource 收发 /
    /// complete_repair / to_process）。`mark_batch_returned` 与
    /// `mark_batch_inspected` 都不写 step，worker-scan 两条分支也**不**推进该列
    /// ⇒ 对多工序链工单它永远停在**首次定位**的那一步，故不可当「当前走到第几步」
    /// 用，只作可选的显示用定位信息。
    ///
    /// 本函数是 `status_gate::apply_batch_status_change` 之上的薄包装
    /// （全仓唯一 `t_part_batch.status` 写入口，写完状态自动补做
    /// part → assembly 派生）。返回 `Result<u64, AppError>`：status_gate 用
    /// `VERSION_CONFLICT` 表达「没写成」，转 `sqlx::Error` 会把 409 降级成 500。
    pub async fn update_batch_dispatched(
        conn: &mut PgConnection,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        updated_by: Option<i64>,
        current_process_id: i64,
    ) -> Result<u64, AppError> {
        crate::modules::prod::batch::status_gate::apply_batch_status_change(
            conn,
            crate::modules::prod::batch::status_gate::StatusChange {
                batch_id,
                new_status: "IN_PROCESS",
                new_location: Some("PRODUCTION_SHELF"),
                new_holder_id: Some(shelf_id),
                new_process_id: Some(current_process_id),
                // 显示用定位信息：dispatch 路径不解析 step，step 写 NULL
                //（由下方 `clear_process_step_id: true` 表达；`new_process_step_id:
                // None` 在 status_gate 里是「不改」，与「清 NULL」是两件事）。
                new_process_step_id: None,
                is_repairing: None,
                expected_version: Some(expected_version),
                // 2026-10-06：源状态白名单含已废弃的 `PROGRAMMING`，与
                // `list_pending_batches` / `preview_auto_dispatch` 的状态闸门同源
                // —— 此处是 `apply_batch_status_change` 的 SQL 层源状态闸门，漏放行
                // 会让 service 层放行的 PROGRAMMING 批次在 UPDATE 阶段被拒（40901）。
                allowed_from: &["PENDING", "PROGRAMMING"],
                updated_by: updated_by.unwrap_or(0),
                // 本包装函数的 `None` 一律是「保持原值」，清空语义由同名
                // clear_* 显式表达。
                clear_location: false,
                clear_holder_id: false,
                clear_process_id: false,
                // **还原**改造前 SQL 的语义 —— 原语句是
                // `current_process_step_id = NULL`（直写）。「dispatch 的批次从未写过
                // step，等价于 NULL」这条等价性依赖一条**没有任何约束保证**的不变式
                // 「`status ∈ {PENDING, PROGRAMMING}` ⇒ step IS NULL」（allowed_from
                // 之外的旁路写点、手工 SQL、历史脏数据都能破坏它），故按
                // 「faithful translation」原则还原为显式清 NULL。
                clear_process_step_id: true,
                // 目标状态 IN_PROCESS 不是终态 → 终态归档事件分支不可达
                event_id: None,
            },
        )
        .await
        .map(|_| 1u64)
    }

    /// 按 chain_id 取工艺链首道 active step。
    ///
    /// `ORDER BY sort_order ASC LIMIT 1`（`sort_order` 是工艺链步骤序号，1-based）。
    /// 0 结果（链已软删 / 步骤被清空）→ `Ok(None)`，由 service 层映射
    /// `AutoDispatch skipped reason='NO_PROCESS_STEP'`。
    pub async fn first_step_of_chain(
        conn: &mut PgConnection,
        chain_id: i64,
    ) -> Result<Option<FirstChainStepRow>, sqlx::Error> {
        let row: Option<FirstChainStepRow> = sqlx::query_as!(
            FirstChainStepRow,
            r#"
            SELECT id AS "step_id!",
                   process_id AS "step_process_id!",
                   sort_order AS "step_sort_order!"
            FROM t_process_chain_step
            WHERE chain_id = $1 AND deleted_at IS NULL
            ORDER BY sort_order ASC, id ASC
            LIMIT 1
            "#,
            chain_id,
        )
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row)
    }

    /// 按 part_id 取 `process_chain_id`（auto-dispatch 路径解析首道 step 前用）。
    ///
    /// 返回 `Ok(None)` 的两种情况：
    /// 1. part 不存在 / 已软删 → service 层映射 `BIZ_PART_NOT_FOUND`
    /// 2. part 存在但 `process_chain_id IS NULL` → service 层映射
    ///    `AutoDispatch skipped reason='NO_PROCESS_CHAIN'`
    pub async fn part_get_process_chain_id(
        conn: &mut PgConnection,
        part_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<Option<i64>> = sqlx::query_scalar(
            r#"
            SELECT process_chain_id
            FROM t_part
            WHERE id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(part_id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.flatten())
    }

    /// 2026-09-30 新增：`auto_dispatch_preview` 单 SQL（取代旧 3 步 SQL）。
    ///
    /// ⚠️ 2026-10-02 域拆分判定：**保留 inline `t_shelf_process` SQL，不抽到
    /// `prod::shelf_process::repo::ShelfProcessRepo`**。该表在下面是
    /// `LEFT JOIN LATERAL` 大复合查询的一部分（与 `t_process_chain_step` 的 LATERAL
    /// 同层），拆成独立子查询会退化成 N+1 往返，是性能回退。`t_shelf_process` 的
    /// SQL 真源已搬到 `prod::shelf_process::repo`，此处仅为 LATERAL 复合查询的
    /// 一处特例内联，谓词（`process_id` + `deleted_at IS NULL` + `sort_order ASC, id
    /// ASC LIMIT 1`）与 `ShelfProcessRepo::find_first_shelf_for_process` 对齐。
    ///
    /// 对每个 `batch_id`：
    /// - 查 part.process_chain_id
    /// - LEFT JOIN LATERAL 取 chain 首道 step（sort_order ASC LIMIT 1）
    /// - LEFT JOIN LATERAL 取该工序的首个货架映射（sort_order ASC LIMIT 1）
    ///
    /// 返回 `Vec<AutoDispatchPreviewRow>`（含 batch_id / part_id / chain_id /
    /// first_process_id / first_process_code / first_process_name / first_shelf_id）。
    ///
    /// 不在结果中的 batch_id 走 service 二次补行 + `skip_reason='NOT_FOUND'`。
    ///
    /// 业务口径：
    /// - 2026-10-06：只取待下发（`IN ('PENDING', 'PROGRAMMING')`）+ 未软删 的 batch，
    ///   与 pending list 端点同一白名单。漏改会让不在 preview 结果里的 batch_id 被
    ///   service 兜底成 `skip_reason='NOT_FOUND'`，前端「自动下发」会把 PROGRAMMING
    ///   批次全判成「批次不存在或状态不可下发」并跳过。
    /// - 不写库，纯只读查询
    /// - 单 SQL 一次扫表，service 主路径不再分 3 步
    ///
    /// 索引命中：`ix_t_part_batch_status`（`status`）+ `pb.deleted_at` 过滤；
    /// LEFT JOIN LATERAL t_process_chain_step 走 `(chain_id, sort_order)` 索引；
    /// LEFT JOIN LATERAL t_shelf_process 走 `(process_id, sort_order)` 索引。
    pub async fn preview_auto_dispatch(
        conn: &mut PgConnection,
        batch_ids: &[i64],
    ) -> Result<Vec<AutoDispatchPreviewRow>, sqlx::Error> {
        if batch_ids.is_empty() {
            return Ok(vec![]);
        }
        let rows: Vec<AutoDispatchPreviewRow> = sqlx::query_as!(
            AutoDispatchPreviewRow,
            r#"
            SELECT
                pb.id            AS "batch_id!",
                p.id             AS "part_id!",
                p.process_chain_id AS "process_chain_id?",
                pcs.process_id   AS "first_process_id?",
                pr.code          AS "first_process_code?",
                pr.name          AS "first_process_name?",
                sp.shelf_id      AS "first_shelf_id?"
            FROM t_part_batch pb
            JOIN t_part p
                ON p.id = pb.part_id AND p.deleted_at IS NULL
            LEFT JOIN LATERAL (
                SELECT process_id
                FROM t_process_chain_step pcs
                WHERE pcs.chain_id = p.process_chain_id
                  AND pcs.deleted_at IS NULL
                ORDER BY pcs.sort_order ASC, pcs.id ASC
                LIMIT 1
            ) pcs ON TRUE
            LEFT JOIN t_process pr
                ON pr.id = pcs.process_id
            LEFT JOIN LATERAL (
                SELECT shelf_id
                FROM t_shelf_process sp
                WHERE sp.process_id = pcs.process_id
                  AND sp.deleted_at IS NULL
                ORDER BY sp.sort_order ASC, sp.id ASC
                LIMIT 1
            ) sp ON TRUE
            WHERE pb.id = ANY($1)
              AND pb.status IN ('PENDING', 'PROGRAMMING')
              AND pb.deleted_at IS NULL
            "#,
            batch_ids as &[i64],
        )
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows)
    }

    /// 按 batch_id 查 part_id（preview_auto_dispatch NOT_FOUND 兜底用）。
    pub async fn find_part_id_by_batch_id(
        conn: &mut PgConnection,
        batch_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT pb.part_id
            FROM t_part_batch pb
            WHERE pb.id = $1
            "#,
        )
        .bind(batch_id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row)
    }
}

// ===== 行结构（SQL FROM 投影） =====

/// `list_pending_batches` JOIN 4 表后的扁平投影（service → vo 转换中间层）。
///
/// 字段全部 `pub` 便于 service 直接读；无 `Serialize`（service 内部使用）。
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct PendingBatchRow {
    // —— t_part_batch ——
    pub pb_id: i64,
    pub pb_part_id: i64,
    pub pb_batch_no: i32,
    pub pb_quantity: i32,
    pub pb_status: String,
    pub pb_version: i32,
    /// `pb_current_process_step_id` PENDING 时通常 NULL；service 投影时用 0
    /// 兜底（与 process_chain_id 同语义：NULL ≡ 0 表示「未设 step」）。
    pub pb_current_process_step_id: Option<i64>,
    pub pb_created_at: chrono::NaiveDateTime,
    // —— t_part ——
    pub p_serial_no: Option<String>,
    pub p_name: String,
    pub p_drawing_no: String,
    pub p_planned_delivery_date: NaiveDate,
    pub p_system_delivery_date: Option<NaiveDate>,
    pub p_is_urgent: bool,
    pub p_note: Option<String>,
    pub p_process_chain_id: Option<i64>,
    // —— t_customer L2 ——
    pub c_name: Option<String>,
    // —— t_customer L1 ——
    pub pc_name: Option<String>,
    // —— t_applicant ——
    pub a_name: Option<String>,
}

/// `first_step_of_chain` 单行投影。
///
/// `t_process_chain_step` 用 `sort_order` 列表示步骤顺序（与 t_shelf_process
/// / t_work_type_process 同名同义）。
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct FirstChainStepRow {
    pub step_id: i64,
    pub step_process_id: i64,
    pub step_sort_order: i32,
}

/// `preview_auto_dispatch` 单行投影（2026-09-30 新增）。
///
/// 字段全为 Option：service 层根据组合判断 skip_reason：
/// - process_chain_id = None → NO_PROCESS_CHAIN
/// - first_process_id = None → NO_PROCESS_STEP
/// - first_shelf_id = None → NO_SHELF（首道工序未映射货架）
/// - 全部齐全 → None（可下发）
#[derive(Debug, sqlx::FromRow)]
pub struct AutoDispatchPreviewRow {
    #[allow(dead_code)]
    pub batch_id: i64,
    #[allow(dead_code)]
    pub part_id: i64,
    pub process_chain_id: Option<i64>,
    pub first_process_id: Option<i64>,
    pub first_process_code: Option<String>,
    pub first_process_name: Option<String>,
    pub first_shelf_id: Option<i64>,
}
