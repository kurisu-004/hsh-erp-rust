//! part 域**唯一**批次状态写入口（status_gate）—— 2026-10-01 新增
//!
//! # 为什么要有这个模块
//!
//! 改造前，27 个写点各自 UPDATE `t_part_batch.status`，然后**靠自觉**再调一次
//! `PartService::sync_from_batch_change` 做 batch → part → assembly 派生。
//! 「要不要顺手调 sync」是一个纯靠人脑维持的约定，实测已经有 3 个写点漏调，
//! 表现是 `t_part.status` / `t_assembly.status` 长期与批次真源不一致 ——
//! 没有任何报错，只是列表页显示错状态。
//!
//! 本模块把「写状态」与「派生」焊死在一个函数里：
//!
//! ```text
//! apply_batch_status_change(conn, StatusChange { .. })
//!   ├─ step 1  UPDATE t_part_batch（OCC + 源状态白名单）  ← 唯一写 status 的地方
//!   ├─ step 2  compute_part_target → UPDATE t_part（派生写，不走 OCC）
//!   ├─ step 3  part.status 真变了 → AssemblyService 反向同步
//!   ├─ step 4  part 进入 COMPLETED/CANCELLED → 先归档 t_part_event 再清 serial_no
//!   └─ step 5  assembly 进入 COMPLETED/CANCELLED → 直接清 serial_no（不归档）
//! ```
//!
//! 13 个历史写点（`mark_batch_*` / `mark_batch_with_status_and_meta` /
//! `mark_batch_status_only` / `PartBatchRepo::update` /
//! `update_batch_dispatched` / 两个 bulk 写点）全部改成本模块之上的**薄包装**，
//! 函数名与签名逐字保留，service 层调用点零改动 —— 于是「写状态」这件事在
//! 类型层面就只剩一个入口，caller 没有「要不要调 sync」这个选项可选。
//!
//! # 与 `PartService::sync_from_batch_change` 的关系
//!
//! step 2–5 抽成本模块的 [`rollup_part_derived`]，`PartService::sync_from_batch_change`
//! 改为一行委托它。两条路径共用同一段派生代码，因此**行为完全一致**：
//! 经 status_gate 写完状态后再调 `sync_from_batch_change` 是安全的冗余
//! （第二次 target == 当前 → NoChange），不会出现两次释放序列号。
//!
//! # 为什么收 `&mut PgConnection` 而不是泛型 `impl PgExecutor<'_>`
//!
//! step 3 必须调 `AssemblyService::sync_from_part_change`（D-6 架构约定：
//! service 不持连接，跨域调用经 `repo.conn_mut()` 拿连接），step 4/5 的
//! 序列号释放要读写 `t_part` / `t_assembly` / `t_part_event` 三张表。
//! 这些都无法表达在 `impl PgExecutor` 上（它不保证能再借出可变的
//! `PgConnection`），故主入口收具体连接。13 个包装函数本来就全是连接入参，
//! 泛型化不带来任何收益。

use sqlx::PgConnection;

use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::assembly::service::{AssemblyService, SyncOutcome};
use crate::modules::part::batch::repo::PartBatchRepo;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::sql::PartRepo;
use crate::modules::part::statemachine::{BatchForRollup, compute_part_target};
use crate::shared::error::{AppError, code};

/// 一次批次状态变更的完整意图（status_gate 的唯一入参形状）。
///
/// 2026-10-01 新增。所有可选列的 `None` 语义统一为 **「保持原值」**
/// （SQL `COALESCE($n, col)`），**唯一例外**是 `new_process_id` + `clear_process_id`
/// 组合（见 `clear_process_id` 字段说明）—— 出池（转 PENDING / INSPECTION）
/// 必须把 `current_process_id` 清 NULL，而「清 NULL」与「保持原值」用单个
/// `Option<i64>` 无法区分。
#[derive(Debug, Clone)]
pub struct StatusChange<'a> {
    /// 目标批次 id。
    pub batch_id: i64,
    /// 目标状态（`PartStatus::as_str()` 字面量）。
    pub new_status: &'a str,
    /// `None` = 保持原值；`Some(loc)` = 写该 location。
    pub new_location: Option<&'a str>,
    /// `None` = 保持原值；`Some(hid)` = 写该 holder（货架 / 工人 / 外协公司）。
    pub new_holder_id: Option<i64>,
    /// `None` = 保持原值（除非 `clear_process_id = true`，此时清 NULL）。
    pub new_process_id: Option<i64>,
    /// `None` = 保持原值。**刻意不提供「清 NULL」语义**：本列是「可选的显示用
    /// 定位信息」（只在首次定位工序时写、之后不推进），全仓没有任何写点需要清它。
    pub new_process_step_id: Option<i64>,
    /// `None` = 保持原值；`Some(b)` = 写返修标记（migration 005）。
    pub is_repairing: Option<bool>,
    /// `Some(v)` = 乐观锁（`WHERE version = $n`），0 行 → 409 `VERSION_CONFLICT`；
    /// `None` = **逃生通道**，跳过 OCC（force-complete 这类强推操作；
    /// 并发串行化由 SQL 行锁承担）。
    pub expected_version: Option<i32>,
    /// 源状态白名单（SQL `status = ANY($n)` 守卫）。
    ///
    /// 放在 SQL 层而不是只靠 service 层 `can_transition_to`，是因为前者与
    /// 写入是**同一条语句的原子条件**，不存在「service 校验通过 → 另一个人
    /// 改掉状态 → 我的 UPDATE 照写」的窗口。
    pub allowed_from: &'a [&'a str],
    /// 写 `updated_by`（同时作为派生列 `t_part.updated_by` / 事件 `created_by`）。
    pub updated_by: i64,
    /// 2026-10-01 新增：`true` 表示本次流转**出池**，`current_process_id`
    /// 必须清 NULL（写不变式第 2 行，见
    /// `migrations/20260930000000_004_add_batch_current_process_id.sql`）。
    ///
    /// 为什么不塞进 `new_process_id`：出池的两个真实写点
    /// （`mark_batch_inspected` / `mark_batch_with_status_and_meta`）语义相反 ——
    /// 前者要**清** `current_process_id` 却要**保留** `current_process_step_id`，
    /// 后者两者都由 caller 显式给定（含「传 None 即清 NULL」的既有约定）。
    /// 一个 `Option` 表达不了三态，独立一个 flag 是这里最省事且不改 13 个包装
    /// 函数签名的做法。
    pub clear_process_id: bool,
}

/// 批量模式的结果（两个 bulk 写点用）。
#[derive(Debug, Clone, Default)]
pub struct BulkSyncOutcome {
    /// 被 UPDATE 命中的**批次**数。0 表示无可覆盖批次 —— 合法，不视为错误
    /// （新建工单未拆批就是 0 行），由 caller 决定。
    pub affected_rows: u64,
    /// 本次受影响并已**逐个完成** part → assembly 派生的 part_id
    /// （去重、升序）。与 `affected_rows` 不同：一次 UPDATE 可能命中同一 part
    /// 下的多条批次。
    pub part_ids: Vec<i64>,
}

/// batch → part → assembly + 终态序列号释放的派生结果。
///
/// `sync` 保持与 `PartService::sync_from_batch_change` 完全一致的返回形状
/// （`Changed(part_id)` / `NoChange`），故 handler 侧的 WS 广播判断零改动。
#[derive(Debug, Clone)]
pub struct RollupOutcome {
    pub sync: SyncOutcome,
    /// `t_part.status` 本次是否**真的**变了（`next_process_id` 单独物化不算）。
    /// 终态序列号释放只在此为 `true` 且新状态是终态时触发。
    pub part_status_changed: bool,
}

/// `SERIAL_RELEASED` 事件类型字面量（`t_part_event.event_type`，varchar(30)）。
pub const EVENT_SERIAL_RELEASED: &str = "SERIAL_RELEASED";

/// **唯一**允许写 `t_part_batch.status` 的入口（单行模式）。
///
/// 在同一事务内完成：批次状态写 → part 派生 → assembly 反向同步 → 终态序列号
/// 归档 / 释放。调用方拿到 `SyncOutcome` 决定是否发 WS 广播。
///
/// 错误码：
/// - 40901 `VERSION_CONFLICT` —— `expected_version` / `allowed_from` 不匹配
///   （0 行）。注意本函数**只**为「批次主操作」抛这个错；派生层（step 2–5）
///   的任何冲突一律降级为 `NoChange`，绝不回滚用户请求的主操作。
/// - 20101 `BIZ_PART_NOT_FOUND` —— part 不存在 / 已软删
/// - 20109 `BIZ_PART_BATCH_NOT_FOUND` —— 批次不存在 / 已软删
pub async fn apply_batch_status_change(
    conn: &mut PgConnection,
    ch: StatusChange<'_>,
) -> Result<SyncOutcome, AppError> {
    let outcome = apply_batch_status_change_detailed(conn, ch).await?;
    Ok(outcome.sync)
}

/// [`apply_batch_status_change`] 的详细版（额外暴露 `part_status_changed`）。
///
/// 给「状态改了但上层还需要知道 part 是否跟着变了」的调用点用；绝大多数调用方
/// 只需要 `sync`，用 `apply_batch_status_change` 即可。
pub async fn apply_batch_status_change_detailed(
    conn: &mut PgConnection,
    ch: StatusChange<'_>,
) -> Result<RollupOutcome, AppError> {
    let part_id = write_batch_status_row(conn, &ch).await?;
    let outcome = rollup_part_derived(conn, part_id, ch.updated_by).await?;
    Ok(outcome)
}

/// step 1：UPDATE `t_part_batch` 单行，返回该行所属 `part_id`。
///
/// 0 行 → `VERSION_CONFLICT`（批次已软删 / version 变了 / 源状态不在白名单）。
///
/// 用 `sqlx::query`（运行时）而非 `query!`（编译期）的原因：这条 SQL 的
/// SET 子句与 WHERE 守卫都随 `StatusChange` 的 `Option` 动态变化
/// （5 个可选列、可选 OCC、`= ANY($n)` 数组白名单），编译期宏无法定型。
/// 运行时 query 是本仓既有做法（见 `phase1/mod.rs::mark_batch_with_status_and_meta`）。
pub(crate) async fn write_batch_status_row(
    conn: &mut PgConnection,
    ch: &StatusChange<'_>,
) -> Result<i64, AppError> {
    if ch.allowed_from.is_empty() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "status_gate: 批次 {} 的 allowed_from 为空，拒绝无条件改写状态",
                ch.batch_id
            ),
        ));
    }
    let row: Option<(i64,)> = sqlx::query_as(
        r#"
        UPDATE t_part_batch
           SET status                  = $2::varchar,
               location                = COALESCE($3::varchar, location),
               current_holder_id       = COALESCE($4::bigint, current_holder_id),
               current_process_id      = CASE WHEN $8::bool THEN NULL
                                              ELSE COALESCE($5::bigint, current_process_id) END,
               current_process_step_id = COALESCE($6::bigint, current_process_step_id),
               is_repairing            = COALESCE($7::boolean, is_repairing),
               version                 = version + 1,
               updated_at              = now(),
               updated_by              = $9::bigint
         WHERE id = $1::bigint
           AND deleted_at IS NULL
           AND status = ANY($10::varchar[])
           AND ($11::int IS NULL OR version = $11::int)
        RETURNING part_id
        "#,
    )
    .bind(ch.batch_id)
    .bind(ch.new_status)
    .bind(ch.new_location)
    .bind(ch.new_holder_id)
    .bind(ch.new_process_id)
    .bind(ch.new_process_step_id)
    .bind(ch.is_repairing)
    .bind(ch.clear_process_id)
    .bind(ch.updated_by)
    .bind(ch.allowed_from)
    .bind(ch.expected_version)
    .fetch_optional(&mut *conn)
    .await?;

    row.map(|(pid,)| pid).ok_or_else(|| {
        AppError::biz(
            code::VERSION_CONFLICT,
            format!(
                "批次 {} 状态未变更（期望源状态 {:?}、version {:?}），或已软删",
                ch.batch_id, ch.allowed_from, ch.expected_version
            ),
        )
    })
}

/// step 2–5：part 派生 + assembly 反向同步 + 终态序列号归档 / 释放。
///
/// `PartService::sync_from_batch_change` 与 status_gate 共用本函数，两者行为
/// 完全一致（前者是「只做派生」的历史入口，后者是「写 + 派生」的合并入口）。
pub async fn rollup_part_derived(
    conn: &mut PgConnection,
    part_id: i64,
    updated_by: i64,
) -> Result<RollupOutcome, AppError> {
    // ---- step 2.1：拉 part 全部活跃批次（rollup 只看活跃行）----
    let batches = PartBatchRepo::list_active_by_part_id(&mut *conn, part_id).await?;
    let rows: Vec<BatchForRollup> = batches
        .iter()
        .map(|b| BatchForRollup {
            status: b.status.clone(),
            location: b.location.clone(),
            current_holder_id: b.current_holder_id,
            current_process_id: b.current_process_id,
        })
        .collect();

    // 空集 → NoChange（防御；PR-B1 已保证每 part 至少有 1 条活跃批次）
    let Some(target) = compute_part_target(&rows) else {
        return Ok(RollupOutcome {
            sync: SyncOutcome::NoChange,
            part_status_changed: false,
        });
    };

    // ---- step 2.2：读 part 当前派生状态 ----
    let cur = PartRepo::get_part_rollup_state(&mut *conn, part_id)
        .await?
        .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在")))?;

    // `target.current_process_id` 已是 process_id（migration 004 起直读
    // `t_part_batch.current_process_id`，不再经 step 中转），直接作为
    // `t_part.next_process_id` 派生缓存写入。
    let derived_next_process_id: Option<i64> = target.current_process_id;

    // target == 当前 → NoChange（不写库、不触发 assembly sync、不释放序列号）
    if cur.status == target.status && cur.next_process_id == derived_next_process_id {
        return Ok(RollupOutcome {
            sync: SyncOutcome::NoChange,
            part_status_changed: false,
        });
    }

    // ---- step 2.3：派生写 `t_part`（**不走 OCC**：派生写，由行锁串行化；
    //      `version += 1` 仍写以保证审计字段单调）----
    let affected = PartRepo::update_part_rollup(
        &mut *conn,
        part_id,
        &target.status,
        derived_next_process_id,
        updated_by,
    )
    .await?;
    if affected == 0 {
        // 防御：part 在两次 select 之间被并发软删。返回 NoChange 让 caller
        // 不重试（与改造前一致）。
        return Ok(RollupOutcome {
            sync: SyncOutcome::NoChange,
            part_status_changed: false,
        });
    }

    let part_status_changed = cur.status != target.status;

    // ---- step 3：part.status 真变了 → assembly 反向同步 ----
    let sync = if part_status_changed {
        AssemblyService::sync_from_part_change_by_id(conn, part_id, updated_by).await?
    } else {
        SyncOutcome::Changed(part_id)
    };

    // ---- step 4：part 进入终态 → 先归档 `t_part_event`，再清 `t_part.serial_no` ----
    //
    // 与「所有批次是否完成」解耦：只要 part 被 rollup 进终态就释放。多批次工单
    // 完成其中一条时，part 未必到终态（min-progress 仍是别的批次）—— 那时
    // **不**释放，等真正到终态的那次 rollup 再释放。序列号因此在 part 的整个
    // 非终态期持续被 `uk_t_part_serial_no` 占用（正确：货还在厂里），不会像
    // 改造前那样因「part 级条件命中 0 行 + `let _ =` 静默吞掉」而永久泄漏。
    //
    // 触发条件是 `part_status_changed && 目标终态`（而非「当前是终态」），
    // 保证「每个 part 最多 1 条 SERIAL_RELEASED」—— 这也是归档事件 id 取
    // `part_id` 的前提（见 `release_part_serial_no`）。
    if part_status_changed && is_terminal(&target.status) {
        release_part_serial_no(conn, part_id, &target.status, updated_by).await?;
    }

    // ---- step 5：assembly 进入终态 → 直接清 `t_assembly.serial_no` ----
    //
    // 父装配件**不**归档：`t_assembly` 没有事件表，而它的 `note` 列是用户可编辑
    // 的业务备注，拿它记系统动作会污染用户数据且事后无法区分。清理条件写在
    // SQL 里（`status IN ('COMPLETED','CANCELLED') AND serial_no IS NOT NULL`），
    // 故对 `sync` 是 `Changed` 还是 `NoChange` 都幂等安全。
    if let SyncOutcome::Changed(assembly_id) = sync {
        clear_assembly_serial_no_if_terminal(conn, assembly_id, updated_by).await?;
    }

    Ok(RollupOutcome {
        sync,
        part_status_changed,
    })
}

/// 批量模式：**唯一**允许一次改写多条 `t_part_batch.status` 的入口。
///
/// 与单行模式的差别只有「受影响的是哪些 part」是未知的 —— 故 UPDATE 用
/// `RETURNING part_id` **在同一条语句内**把集合捞出来（理由见下），随后
/// 对去重后的每个 part_id 各跑一次 [`rollup_part_derived`]。
///
/// ## 为什么用 `RETURNING` 而不是「先 SELECT 受影响 part 再 UPDATE」或「让 caller 回传」
///
/// 1. **原子性**：受影响的批次集合与 UPDATE 在同一条语句、同一快照内确定。
///    「先读后写」在 READ COMMITTED 下有 TOCTOU 窗口（读完到写之间别的 tx
///    可能插批次 / 改状态），cancel 路径尤其危险（可能把新挂上送货单的批次
///    一起拖进 CANCELLED）。
/// 2. **caller 签名不变**：两个 bulk 写点的返回值语义是「影响行数」，
///    `RETURNING` 让内部能拿到 part_id 集合而不必把 `Vec<i64>` 泄漏给 service 层。
/// 3. **不重复 rollup**：内部对 `part_id` 排序去重，同一 part 下的 N 条批次
///    只派生一次（rollup 本身幂等，但重复调用会多打 2N 次 SELECT）。
///
/// `excluded_statuses` 是**反向**白名单（终态保护）：cancel 传
/// `["COMPLETED", "CANCELLED"]`，force-complete 传 `["CANCELLED"]`。
/// 用反向而非正向，是因为 force-complete 的语义就是「除终态外全部强推」，
/// 写成正向白名单需要枚举 9 个非终态值，新增状态时必然漏改。
pub async fn apply_bulk_batch_status_change_for_part(
    conn: &mut PgConnection,
    part_id: i64,
    new_status: &str,
    excluded_statuses: &[&str],
    updated_by: i64,
) -> Result<BulkSyncOutcome, AppError> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        r#"
        UPDATE t_part_batch
           SET status     = $2::varchar,
               version    = version + 1,
               updated_at = now(),
               updated_by = $3::bigint
         WHERE part_id = $1::bigint
           AND deleted_at IS NULL
           AND NOT (status = ANY($4::varchar[]))
        RETURNING part_id
        "#,
    )
    .bind(part_id)
    .bind(new_status)
    .bind(updated_by)
    .bind(excluded_statuses)
    .fetch_all(&mut *conn)
    .await?;

    let affected_rows = rows.len() as u64;
    let mut part_ids: Vec<i64> = rows.into_iter().map(|(pid,)| pid).collect();
    part_ids.sort_unstable();
    part_ids.dedup();

    for pid in &part_ids {
        rollup_part_derived(&mut *conn, *pid, updated_by).await?;
    }

    Ok(BulkSyncOutcome {
        affected_rows,
        part_ids,
    })
}

// ---------- step 4 / step 5：终态序列号释放 ----------

/// 子件（`t_part`）终态序列号释放：**先归档后清**。
///
/// 归档而非直接清的理由：序列号转交送货单后要从工单上消失，直接清会丢失
/// 「这个工单曾经用过哪个序列号」这条审计链。`t_part_event` 正是为此存在。
///
/// 事件 id 用 `part_id`（不是雪花）：repo 层没有雪花生成器（`SnowflakeIdGenerator`
/// 由 `main.rs` 持有并逐层透传，repo 方法一律收显式 id，见
/// `insert_child_for_assembly`），而本事件**每个 part 最多 1 条**
/// （COMPLETED / CANCELLED 都是终态，不可能重复进入）。`part_id` 本身就是一个
/// 真实雪花值、由同一套分配流发出，永不等于任何运行时生成的 `t_part_event.id`
/// （同一分配流内不重复、跨 instance 由 10 位 instance_id 隔离），
/// 且让整段逻辑**确定性、可重放**。
async fn release_part_serial_no(
    conn: &mut PgConnection,
    part_id: i64,
    new_status: &str,
    updated_by: i64,
) -> Result<(), AppError> {
    // 先读原值（归档内容需要它）
    let serial: Option<(Option<String>,)> =
        sqlx::query_as("SELECT serial_no FROM t_part WHERE id = $1 AND deleted_at IS NULL")
            .bind(part_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((Some(original),)) = serial else {
        // 已被别的路径清空（如 `mark_part_cancelled` 直接置 NULL）→ 无需归档
        return Ok(());
    };

    // step 4.1：归档（先写事件，再清列 —— 顺序不能反：反了万一清列成功、
    // 事件写失败，事务虽回滚但语义上「已释放却无记录」的窗口更难推理）
    let note = format!("序列号释放归档：{original}");
    PartRepo::insert_part_event(
        &mut *conn,
        NewPartEvent {
            id: part_id,
            part_id,
            event_type: EVENT_SERIAL_RELEASED,
            from_status: None,
            to_status: Some(new_status),
            batch_id: None,
            quantity: None,
            drawing_code: None,
            badge_code: None,
            note: Some(&note),
            created_by: Some(updated_by),
        },
    )
    .await?;

    // step 4.2：清列（谓词含 `serial_no IS NOT NULL` + 终态，天然幂等）
    sqlx::query(
        "UPDATE t_part SET serial_no = NULL, version = version + 1, \
         updated_at = now(), updated_by = $2 \
         WHERE id = $1 AND serial_no IS NOT NULL AND deleted_at IS NULL \
           AND status IN ('COMPLETED', 'CANCELLED')",
    )
    .bind(part_id)
    .bind(updated_by)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 父装配件（`t_assembly`）终态序列号释放：**直接清，不归档**（理由见上）。
async fn clear_assembly_serial_no_if_terminal(
    conn: &mut PgConnection,
    assembly_id: i64,
    updated_by: i64,
) -> Result<(), AppError> {
    AssemblyRepo::clear_serial_no_if_terminal(&mut *conn, assembly_id, updated_by).await?;
    Ok(())
}

#[inline]
fn is_terminal(status: &str) -> bool {
    matches!(status, "COMPLETED" | "CANCELLED")
}
