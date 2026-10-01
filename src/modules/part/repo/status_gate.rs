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
//! ## 唯一签名例外：3 个 `mark_batch_*` 返回 `RollupOutcome`（2026-10-01 补记）
//!
//! `mark_batch_passed_inspection` / `mark_batch_inspected` /
//! `mark_batch_failed_inspection` 的返回值由 `u64` 改成 [`RollupOutcome`]。
//! 原因：这三者的调用点（`inspection_core.rs` 3 处 + `worker_scan.rs` 1 处）
//! 要把 `SyncOutcome` 填进响应的 `synced_assembly_id`，handler 再据此发
//! `ASSEMBLY_UPDATED` 广播。而 gate 已经在**同一次调用**里做完 part 派生 +
//! assembly 反向同步，service 若再补调一次 `PartService::sync_from_batch_change`，
//! 第二次 rollup 必然 `NoChange`（target 已 == 当前）——`synced_assembly_id`
//! 会被**恒为 null** 吞掉，广播随之永不发（真回归，非理论问题：
//! `tests/assembly/status_sync.rs` 三个用例就是被它打红的）。
//! 其余 10 个包装函数仍返回原类型（`u64` / `BulkSyncOutcome` / 无）：它们的
//! 调用点不消费派生结果。
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

/// =============================================================================
/// CI 源码护栏：除本文件外，任何 `.rs` 都不得写 `t_part_batch.status`
/// =============================================================================
///
/// ## 这个测试防的是什么（真实故障类别，不是风格洁癖）
///
/// 改造前 27 个写点各自 UPDATE `t_part_batch.status`，batch → part → assembly
/// 的派生要靠「**记得**再调一次 `PartService::sync_from_batch_change`」维持。
/// 这条约定没有任何机制支撑，实测 3 个写点漏调：功能上完全正常、没有任何报错、
/// 测试也全绿，只是 `t_part.status` / `t_assembly.status` 从此长期与真源不一致，
/// 表现为列表页显示错状态。用户看到的是「这单明明还在厂里怎么显示已交付」。
///
/// 2026-10-01 的 status_gate 改造把写与派生焊进 [`apply_batch_status_change`]
/// 之后，「漏调」这个选项
/// 从类型层面消失了；但**绕过** status_gate 直接写一行的能力还在（任何人拿
/// `sqlx::query` 手写 UPDATE 都不受编译期约束）。这个测试就是那道约束的
/// 持续执行者：把「不该出现的写法」变成 CI 里的一条红线。
///
/// ## 放在 lib 单测（`cargo test --lib`）而不是 `tests/` 的理由
///
/// 这是**源码级不变量**，与 DB / Redis / 迁移 / fixture 全都无关：
/// - 放 `tests/` 会落进 20 个需要 per-test fresh database 的 integration binary，
///   白白让「跑个护栏」依赖一整套容器基建，且 nextest 的 `-E` 过滤也更容易被漏掉；
/// - lib 的 `#[cfg(test)]` 模块被 `cargo test` / `cargo nextest run --lib`
///   无条件执行，不需要任何命令行参数、不需要起容器，几百毫秒内完成 —— 于是
///   「忘了这茬」的成本降到 0，它是真能当日常 CI 闸门的东西。
/// - 它也正好住在被保护的那扇门（`status_gate.rs`）里，规则的 rationale 与规则本身
///   写在一起，后人改这个模块时必然读到。
///
/// ## 判定规则
///
/// 一个文件被判为「写了批次状态」当且仅当**同时**满足：
/// 1. 出现 `UPDATE t_part_batch`（大小写不敏感，允许 `UPDATE` 与表名之间有换行 /
///    空白；也允许 `UPDATE t_part_batch pb` 这种带别名的写法）；
/// 2. 该语句的 **SET 子句**（`SET` 与 `WHERE` 之间，或 `SET` 与语句末尾 `;` 之间）
///    存在对 `status` 列的赋值（`status =` / `status=`，允许 `pb.status =` 这种
///    带表限定名的写法）。
///
/// 只看 SET 子句这一条很关键，它同时排除掉两类天然会出现的干扰：
/// - `SELECT ... FOR UPDATE` 之类的**只读**语句：根本没有 SET 子句；
/// - `UPDATE t_part_batch SET delivery_note_id = ...` / `SET quantity = quantity - $n` /
///   `SET current_holder_id = ...` 等**只改其它列**的合法写点（2026-10-01 实测全仓
///   10 处，分布在 worker_pool 抢占 / move 归还 / 批次挂送货单 / 拆批扣量 /
///   `mark_batch_returned` 归还货架 / part 发料台定位）——它们命中条件 1 但不命中
///   条件 2，不该被拦。
#[cfg(test)]
mod write_guard_tests {
    use std::path::{Path, PathBuf};

    /// 全仓唯一被允许写 `t_part_batch.status` 的文件（相对 crate 根）。
    ///
    /// 写死成字面量而不是 `file!()`：`file!()` 只能证明「本文件自己干净」，
    /// 而这条规则要表达的是「**别的**文件不许写」，两者不是一回事。
    const SANCTIONED: &str = "src/modules/part/repo/status_gate.rs";

    /// 扫描用的字面量。
    ///
    /// 2026-10-01：`NEEDLE_TABLE_WORD` 拆成两段 `concat!` 是刻意的 —— 万一将来
    /// 有人把这个 `mod` 从本文件搬走，源码里就不会出现完整可匹配的表名
    /// （`"t_part_", "batch"` 中间有引号与逗号），测试不会把自己的检测器当成
    /// 违规者（自指误报）。
    ///
    /// 匹配用「两个词 + 中间任意空白」的形式（`update` 后可跟任意个空格 / 换行
    /// / 制表符再接表名），因为真实 SQL 里的这条语句经常被 rustfmt 或手写换行
    /// 拆成 `UPDATE\n  t_part_batch`（见本文件 `write_batch_status_row`）。
    const NEEDLE_UPDATE_WORD: &str = "update";
    const NEEDLE_TABLE_WORD: &str = concat!("t_part_", "batch");

    /// 字符分类（`scan_rust` 产出）。
    const KIND_OTHER: u8 = 0; // 空白 / 注释（注释已被空格化）
    const KIND_CODE: u8 = 1; // 代码字符（花括号配平只认它）
    const KIND_STR: u8 = 2; // 字符串 / 字符字面量内容（含引号）

    /// 把 Rust 源码切成「注释已空格化」的文本 + 每字节分类。
    ///
    /// 注释空格化而不是整段删除，是为了让**行号保持不变**，失败信息里能给出
    /// 精确的 `file:line`。
    ///
    /// 字面量识别的必要性：SQL 本身就住在字符串里，不能被当注释抹掉；但
    /// 字符串里的 `{`（SQL 里的 `$`、JSON 模板等）又不该参与 `#[cfg(test)]`
    /// 块的括号配平。分类后两个需求各取所需。
    fn scan_rust(src: &str) -> (Vec<u8>, Vec<u8>) {
        let b = src.as_bytes();
        let mut out = b.to_vec();
        let mut kind = vec![KIND_OTHER; b.len()];
        let mut i = 0usize;
        while i < b.len() {
            let c = b[i];
            // ---- 行注释 ----
            if c == b'/' && b.get(i + 1) == Some(&b'/') {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            // ---- 块注释（Rust 支持嵌套）----
            if c == b'/' && b.get(i + 1) == Some(&b'*') {
                let mut depth = 1usize;
                while i < b.len() {
                    if b[i] == b'\n' {
                        i += 1;
                        continue;
                    }
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        continue;
                    }
                    if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                        continue;
                    }
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            // ---- raw string / byte string 前缀（`r"` `r#"` `b"` `br#"`，可叠加 c）----
            if let Some((lit_start, body_start)) = raw_string_start(b, i) {
                let Some(close) = raw_string_end(b, body_start, lit_start) else {
                    // 理论不可达（源码一定编译得过）；保守当普通代码处理，避免死循环
                    kind[i] = KIND_CODE;
                    i += 1;
                    continue;
                };
                for slot in kind.iter_mut().take(close + 1).skip(i) {
                    *slot = KIND_STR;
                }
                i = close + 1;
                continue;
            }
            // ---- 普通字符串（含 `b"..."` 的引号起点）----
            if c == b'"' {
                let mut j = i + 1;
                while j < b.len() {
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == b'"' || b[j] == b'\n' {
                        break;
                    }
                    j += 1;
                }
                let end = j.min(b.len() - 1);
                for slot in kind.iter_mut().take(end + 1).skip(i) {
                    *slot = KIND_STR;
                }
                i = end + 1;
                continue;
            }
            // ---- 字符字面量 vs 生命周期（`'a` vs `'x'`）----
            if c == b'\'' {
                let is_char = match b.get(i + 1) {
                    // `'\\n'` 形式：反斜杠开头一定不是生命周期
                    Some(b'\\') => true,
                    // `'x'` 恰好三字节
                    Some(_) if b.get(i + 2) == Some(&b'\'') => true,
                    _ => false,
                };
                if is_char {
                    let mut j = i + 1;
                    while j < b.len() {
                        if b[j] == b'\\' {
                            j += 2;
                            continue;
                        }
                        if b[j] == b'\'' {
                            break;
                        }
                        j += 1;
                    }
                    let end = j.min(b.len() - 1);
                    for slot in kind.iter_mut().take(end + 1).skip(i) {
                        *slot = KIND_STR;
                    }
                    i = end + 1;
                    continue;
                }
                // 生命周期标注（`&'a mut T`）→ 普通代码
                kind[i] = KIND_CODE;
                i += 1;
                continue;
            }
            if !c.is_ascii_whitespace() {
                kind[i] = KIND_CODE;
            }
            i += 1;
        }
        (out, kind)
    }

    /// 判断 `b[i..]` 是否是 raw string / byte string 的开头。
    ///
    /// 返回 `(字面量起始下标, 内容起始下标)`；`r` / `b` / `c` 前缀本身算内容
    /// （一并标成 KIND_STR 即可，prefix 里没有括号）。
    fn raw_string_start(b: &[u8], i: usize) -> Option<(usize, usize)> {
        let mut p = i;
        // 前缀可任意组合（`br` / `cr` / `rb` …），只要最终紧跟 `"` 或 `r#"`
        while matches!(b.get(p), Some(b'r') | Some(b'b') | Some(b'c')) {
            p += 1;
        }
        let is_raw = b.get(p) == Some(&b'r') && p > i;
        let q = if is_raw { p + 1 } else { p };
        match b.get(q) {
            Some(b'"') => Some((i, q)),
            Some(b'#') => {
                let mut h = q;
                while b.get(h) == Some(&b'#') {
                    h += 1;
                }
                if b.get(h) == Some(&b'"') {
                    Some((i, h + 1))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// 从 raw string 内容起点找闭合 `"#…#`；返回闭合引号下标。
    fn raw_string_end(b: &[u8], body_start: usize, _lit_start: usize) -> Option<usize> {
        // 内容起点的连续 `#` 个数 = 闭合时需要匹配的 `#` 个数
        let mut hashes = 0usize;
        let mut p = body_start;
        while b.get(p) == Some(&b'#') {
            hashes += 1;
            p += 1;
        }
        if b.get(p) != Some(&b'"') {
            return None;
        }
        p += 1;
        while p < b.len() {
            if b[p] == b'"' {
                let mut h = 0usize;
                while b.get(p + 1 + h) == Some(&b'#') {
                    h += 1;
                }
                if h == hashes {
                    return Some(p + h);
                }
                p += 1;
                continue;
            }
            p += 1;
        }
        None
    }

    /// 标出所有 `#[cfg(test)]` 块的字节区间（含属性行到配平大括号）。
    ///
    /// 括号配平只数 `KIND_CODE` 的 `{` / `}`，字符串里的括号不参与。
    fn cfg_test_regions(out: &[u8], kind: &[u8]) -> Vec<bool> {
        let mut region = vec![false; out.len()];
        let lower: Vec<u8> = out.iter().map(|c| c.to_ascii_lowercase()).collect();
        let needle = b"cfg";
        let mut from = 0usize;
        while let Some(rel) = find_sub(&lower[from..], needle) {
            let at = from + rel;
            from = at + needle.len();
            // `cfg` 与 `(` 之间、`test` 与 `)` 之间都允许空白（rustfmt 不动 cfg 属性内部）
            let mut p = skip_ws(&lower, at + needle.len());
            if lower.get(p) != Some(&b'(') {
                continue;
            }
            p = skip_ws(&lower, p + 1);
            if !lower[p..].starts_with(b"test") {
                continue;
            }
            p = skip_ws(&lower, p + 4);
            if lower.get(p) != Some(&b')') {
                continue;
            }
            p = skip_ws(&lower, p + 1);
            if lower.get(p) != Some(&b']') {
                continue;
            }
            p += 1;
            // 属性后可能还跟别的属性或空白；下一个**代码**位置的 `{` 即块首
            let Some(open) = find_code_byte(out, kind, p, b'{') else {
                continue;
            };
            let mut depth = 0i32;
            let mut k = open;
            while k < out.len() {
                if kind[k] == KIND_CODE {
                    if out[k] == b'{' {
                        depth += 1;
                    } else if out[k] == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                k += 1;
            }
            for r in region.iter_mut().take(k).skip(at) {
                *r = true;
            }
        }
        region
    }

    fn skip_ws(b: &[u8], mut i: usize) -> usize {
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        i
    }

    fn find_code_byte(out: &[u8], kind: &[u8], from: usize, target: u8) -> Option<usize> {
        (from..out.len()).find(|&i| kind[i] == KIND_CODE && out[i] == target)
    }

    fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || hay.len() < needle.len() {
            return None;
        }
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// 忽略大小写找「前一个字符不是标识符字符」的子串（`word boundary` 变体）。
    fn find_word(hay: &[u8], needle_lower: &[u8]) -> Option<usize> {
        let mut from = 0usize;
        while let Some(rel) = find_sub(&hay[from..], needle_lower) {
            let at = from + rel;
            from = at + 1;
            let prev_ok = at == 0 || !is_ident(hay[at - 1]);
            let next_ok = !hay
                .get(at + needle_lower.len())
                .is_some_and(|c| is_ident(*c));
            if prev_ok && next_ok {
                return Some(at);
            }
        }
        None
    }

    fn is_ident(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
    }

    /// 判定一条被剥掉注释的文本里是否有「对 status 列的赋值」。
    ///
    /// 允许 `status =` / `status=`，也允许 `pb.status =`（限定名）。
    fn set_clause_assigns_status(slice_lower: &[u8]) -> bool {
        let target = b"status";
        let mut from = 0usize;
        while let Some(rel) = find_sub(&slice_lower[from..], target) {
            let at = from + rel;
            from = at + target.len();
            // 前一个字符必须是标识符边界（`.` 也算边界，允许 `pb.status`）
            let prev_ok = at == 0 || !is_ident(slice_lower[at - 1]);
            // 后跟可选空白 + `=`，且 `=` 之后不是 `=`（排除 `status ==`）
            let p = skip_ws(slice_lower, at + target.len());
            if slice_lower.get(p) != Some(&b'=') {
                continue;
            }
            if slice_lower.get(p + 1) == Some(&b'=') {
                continue;
            }
            if prev_ok {
                return true;
            }
        }
        false
    }

    /// 找「`update` + 任意空白 + 表名」的起点（1 基行号由调用方换算）。
    ///
    /// 分两步而不是一次 `find_sub`：一次匹配表达不了中间「任意多个空白」。
    fn find_update_table(hay: &[u8], from: usize) -> Option<usize> {
        let mut at = from;
        while let Some(rel) = find_word(&hay[at..], NEEDLE_UPDATE_WORD.as_bytes()) {
            let u = at + rel;
            let t = skip_ws(hay, u + NEEDLE_UPDATE_WORD.len());
            if hay[t..].starts_with(NEEDLE_TABLE_WORD.as_bytes())
                && !hay
                    .get(t + NEEDLE_TABLE_WORD.len())
                    .is_some_and(|c| is_ident(*c))
            {
                return Some(u);
            }
            at = u + 1;
        }
        None
    }

    /// 返回一个文件里所有「写 `t_part_batch.status`」的位置（1 基行号）。
    fn status_write_lines(src: &str) -> Vec<usize> {
        let (out, kind) = scan_rust(src);
        let lower: Vec<u8> = out.iter().map(|c| c.to_ascii_lowercase()).collect();
        let test_region = cfg_test_regions(&out, &kind);
        let mut hits = Vec::new();
        let mut from = 0usize;
        while let Some(at) = find_update_table(&lower, from) {
            from = at + 1;
            if test_region[at] {
                continue; // 见下方「假阳性处理 (b)」
            }
            // 语句边界：下一个 `;`（找不到就取末尾）
            let stmt_end = lower[at..]
                .iter()
                .position(|c| *c == b';')
                .map(|p| at + p)
                .unwrap_or(lower.len());
            let Some(set_at) = find_word(&lower[at..stmt_end], b"set").map(|p| at + p) else {
                continue;
            };
            let set_body_start = set_at + 3;
            let set_body_end = find_word(&lower[set_body_start..stmt_end], b"where")
                .map(|p| set_body_start + p)
                .unwrap_or(stmt_end);
            if set_clause_assigns_status(&lower[set_body_start..set_body_end]) {
                hits.push(1 + lower[..at].iter().filter(|c| **c == b'\n').count());
            }
        }
        hits
    }

    /// 递归列 `dir` 下的全部 `.rs`（目录深度不限）。
    fn collect_rs(dir: &Path, acc: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        // 排序只为让失败信息稳定可读
        entries.sort();
        for path in entries {
            if path.is_dir() {
                collect_rs(&path, acc);
            } else if path.extension().is_some_and(|e| e == "rs") {
                acc.push(path);
            }
        }
    }

    /// 假阳性处理政策（三类，逐条写明理由）：
    ///
    /// (a) **注释 / 文档注释里提到这条 SQL** —— 排除。`scan_rust` 把注释
    ///     空格化后才做匹配。本仓有 8 处文档注释在描述「UPDATE t_part_batch
    ///     : INSPECTION → READY_TO_SHIP（OCC）」这类流程（`inspection_core.rs`
    ///     3 处、`batch_sql.rs` 3 处、`worker_pool/repo/sql.rs` 1 处），它们是
    ///     **文档**，排除掉之后规则才能盯住真实 SQL。
    ///
    /// (b) **`#[cfg(test)]` 块** —— 排除。全仓唯一的真实例子是
    ///     `src/modules/prod/batch/service.rs::dispatch_batch_concurrent_modification_collects_invalid_status_failure`
    ///     里的 `UPDATE t_part_batch SET status='IN_PROCESS', version=99`：
    ///     它故意把 version 顶到 99 来**伪造一次并发改动**。`StatusChange` 的
    ///     OCC 只会 `version + 1`，表达不了「凭空跳到 99」，所以这条 fixture
    ///     无论怎么改都过不了 gate；硬要它走生产 API 只会把测试写得比生产代码
    ///     还绕。护栏要防的是**生产写路径**漏派生，不是禁止单测造数据。
    ///
    /// (c) **只读语句 / 只改其它列的 UPDATE** —— 排除，规则只看 SET 子句。
    ///     `SELECT ... FOR UPDATE` 没有 SET 子句；`SET delivery_note_id` /
    ///     `SET quantity` / `SET current_holder_id` / `SET current_process_id`
    ///     虽命中 `UPDATE t_part_batch` 但 SET 子句里没有对 `status` 的赋值，
    ///     合法放行。
    #[test]
    fn no_outside_file_writes_batch_status() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        collect_rs(&root.join("src"), &mut files);
        assert!(
            !files.is_empty(),
            "扫描不到任何 .rs（CARGO_MANIFEST_DIR={}），护栏本身失效",
            root.display()
        );

        let mut violations: Vec<String> = Vec::new();
        for path in &files {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if rel == SANCTIONED {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            for line in status_write_lines(&src) {
                violations.push(format!("  {rel}:{line}"));
            }
        }

        assert!(
            violations.is_empty(),
            "以下 {} 处直接写了 `t_part_batch.status`，绕过了 part 域唯一状态写入口 \
             `src/modules/part/repo/status_gate.rs`：\n{}\n\
             \n\
             规则（见 `status_gate.rs` 末尾 `mod write_guard_tests`）：\n\
             \x20 * 判定 = 同一语句里既有对批次表的 UPDATE、其 SET 子句又对 `status` 列赋值；\n\
             \x20 * 注释 / `#[cfg(test)]` 块内、以及只改其它列的 UPDATE 不在判定范围内。\n\
             \n\
             正确写法：改用 `status_gate::apply_batch_status_change`（单行）或\n\
             `apply_bulk_batch_status_change_for_part`（批量）。它们在一个函数内完成\n\
             「写批次状态 → 回流 `t_part.status` / `next_process_id` → 级联 `t_assembly.status`\n\
             → 终态序列号归档 / 释放」，因此 caller 不需要、也不应该自己补调任何 sync ——\n\
             自行动手补调不但冗余，还会让响应里的 `synced_assembly_id` 恒为 null。",
            violations.len(),
            violations.join("\n")
        );
    }

    #[test]
    fn guard_detector_actually_flags_a_violation() {
        // 元测试：护栏本身必须能报警，否则「全绿」只是因为探测器瞎了。
        // 片段用 `concat!` 之外的裸字面量是**故意的** —— 它不在被扫描的 src/ 树里。
        let bad = r#"
            fn demo(conn: &mut PgConnection) {
                sqlx::query("UPDATE t_part_batch \
                     SET status = $2, version = version + 1 WHERE id = $1")
                    .execute(conn)
                    .await
                    .unwrap();
            }
        "#;
        assert_eq!(status_write_lines(bad), vec![3]);

        // 只改其它列 → 不该报警
        let ok_other_col = r#"
            fn demo(conn: &mut PgConnection) {
                sqlx::query("UPDATE t_part_batch SET delivery_note_id = $2 WHERE id = $1")
                    .execute(conn)
                    .await
                    .unwrap();
            }
        "#;
        assert!(status_write_lines(ok_other_col).is_empty());

        // 注释里提到 → 不该报警
        let ok_comment = r#"
            // 5. UPDATE t_part_batch: INSPECTION → READY_TO_SHIP（OCC + 写 updated_by）
            /* UPDATE t_part_batch SET status = 'INSPECTION' WHERE id = $1 */
            fn demo() {}
        "#;
        assert!(status_write_lines(ok_comment).is_empty());

        // SELECT ... FOR UPDATE → 不该报警
        let ok_select = r#"
            fn demo(conn: &mut PgConnection) {
                sqlx::query("SELECT status FROM t_part_batch WHERE id = $1 FOR UPDATE")
                    .fetch_one(conn)
                    .await
                    .unwrap();
            }
        "#;
        assert!(status_write_lines(ok_select).is_empty());

        // #[cfg(test)] 块内 → 不该报警
        let ok_cfg_test = r#"
            fn demo() {}
            #[cfg(test)]
            mod tests {
                async fn force() {
                    sqlx::query("UPDATE t_part_batch SET status='IN_PROCESS', version=99 WHERE id=$1")
                        .execute(conn).await.unwrap();
                }
            }
        "#;
        assert!(status_write_lines(ok_cfg_test).is_empty());

        // 大小写 / 换行 / 别名 / 限定名都要能命中
        let tricky = "let q = \"update   t_part_batch\\n  set pb.status= $1 where id=$2\";";
        assert_eq!(status_write_lines(tricky), vec![1]);
    }
}
