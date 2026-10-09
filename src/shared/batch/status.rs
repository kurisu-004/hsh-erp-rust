//! **唯一**批次状态写入口
//!
//! 2026-10-08 自 `prod::batch::status_gate` 上移到 shared 层：派生链跨 part /
//! assembly / batch 三域，与 `shared::batch` 模块 doc 的边界小节同因。
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
//!   ├─ step 2  compute_part_target → UPDATE t_part（派生写，不走 OCC、**带终态守卫**）
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
//! ## 两条派生层铁律（2026-10-01 review 第 1 轮 B1 补齐）
//!
//! 1. **派生层不得否决主操作**：派生写不抛错，冲突 / 守卫命中一律降级为
//!    `NoChange`（见 `sync_assembly_status` 的 OCC 降级段）。
//! 2. **派生层不得覆盖主操作**（B1 的教训）：`t_part` 已由主操作
//!    （`PartService::cancel` → `mark_part_cancelled`）写成终态时，min-progress
//!    再算出别的状态也**不许写进去**。SQL 层的终态守卫
//!    （`update_part_rollup` 的 `status NOT IN ('COMPLETED','CANCELLED')`）是
//!    兜底，bulk 入口的 [`PartDerivation::KeepPartTerminalAsIs`] 是显式表达 ——
//!    后者保证「跳过 part 写」的同时**继续**派生父装配件。
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
//! 经本模块写完状态后再调 `sync_from_batch_change` 是安全的冗余
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
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::sql::PartRepo;
use crate::modules::part::statemachine::{BatchForRollup, compute_part_target};
use crate::shared::batch::read::list_active_batches_by_part_id;
use crate::shared::error::{AppError, code};

/// 一次批次状态变更的完整意图（本模块的唯一入参形状）。
///
/// 2026-10-01 新增。**所有可选列的 `None` 语义统一为「保持原值」**
/// （SQL `COALESCE($n, col)`），「清 NULL」一律由同名 `clear_*` 标志显式表达。
/// 单个 `Option` 表达不了「保持 / 写值 / 清空」三态，把「清」混进 `None`
/// 会让同一个 `None` 在不同调用点有两种含义 —— 2026-10-01 review 第 1 轮 M2
/// 就是这么把「出池清 `current_process_step_id`」悄悄改成「保持原值」的。
#[derive(Debug, Clone)]
pub struct StatusChange<'a> {
    /// 目标批次 id。
    pub batch_id: i64,
    /// 目标状态（`PartStatus::as_str()` 字面量）。
    pub new_status: &'a str,
    /// `None` = 保持原值（除非 `clear_location`）；`Some(loc)` = 写该 location。
    pub new_location: Option<&'a str>,
    /// `None` = 保持原值（除非 `clear_holder_id`）；`Some(hid)` = 写该 holder。
    pub new_holder_id: Option<i64>,
    /// `None` = 保持原值（除非 `clear_process_id`）。
    pub new_process_id: Option<i64>,
    /// `None` = 保持原值（除非 `clear_process_step_id`）。
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
    /// `true` = `location` 必须清 NULL。
    ///
    /// **2026-10-01 review 第 1 轮 M2 新增**：出池 / 召回等流转必须把
    /// 「压在哪个架子上」一起清掉，否则 UI 上「待投产」的工单仍显示
    /// `PRODUCTION_SHELF` + `current_holder_id`（改造前 `mark_batch_with_status_
    /// and_meta` 的 `location = $4` 传 None 就是**写 NULL**，本轮一度被误改成
    /// 「保持原值」）。
    pub clear_location: bool,
    /// `true` = `current_holder_id` 必须清 NULL（同 M2，理由见上）。
    pub clear_holder_id: bool,
    /// `true` = `current_process_id` 必须清 NULL（写不变式第 2 行，见
    /// `migrations/20260930000000_004_add_batch_current_process_id.sql`）。
    ///
    /// 为什么单独一个 flag 而不是塞进 `new_process_id`：出池要**清**
    /// `current_process_id`，此时是否连带清 `current_process_step_id` 由 caller 独立
    /// 决定（两者不必同进同出），一个 `Option` 表达不了三态。
    pub clear_process_id: bool,
    /// `true` = `current_process_step_id` 必须清 NULL。
    ///
    /// **2026-10-01 review 第 1 轮 M2 新增**：送检（`INSPECTION`）/ 外协收回 /
    /// 召回等**出池**写点在改造前都把该列清成 NULL。展示用的 `next_process_id`
    /// 正是由它经 `t_process_chain_step JOIN` 派生，不清会让「已出池的批次仍
    /// 显示上一道工序」。
    pub clear_process_step_id: bool,
    /// `SERIAL_RELEASED` 归档事件（step 4）的主键。`None` = 调用方拿不到雪花
    /// 生成器（见 [`rollup_part_derived`] 的说明），此时**只清序列号、不写归档
    /// 事件**并打 `error!`。
    ///
    /// 2026-10-01 review 第 1 轮 M4 新增：此前实现直接拿 `part_id` 当事件 id，
    /// 而 `GET /parts/{id}/events` 按 `id DESC` 排序 —— `part_id` 是**建单时**的
    /// 雪花，比该 part 后续所有事件小若干个数量级，归档事件会被排到时间线
    /// **最底部**，看起来像建单时就发生过；且一旦 part 离开终态再回来，第二次
    /// 插入就是 pkey 冲突 → 整个事务 500。
    pub event_id: Option<i64>,
}

/// 批量入口对 `t_part.status` 这层**派生缓存**的处置策略。
///
/// 2026-10-01 review 第 1 轮 B1 新增。存在理由：`PartService::cancel` 的
/// 「取消工单」是**主操作**（`mark_part_cancelled` 直接把 `t_part.status`
/// 写成 CANCELLED），紧随其后的批次级联**不允许**再按 min-progress 把它推回去。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartDerivation {
    /// 正常：批次是真源，`t_part.status` 按 min-progress 派生。
    Rollup,
    /// `t_part.status` 已由**主操作**打成终态，一个字都不许碰；
    /// 本次只写批次，**并继续向下派生父装配件**（否则父件会与子件长期不一致）。
    ///
    /// 为什么还需要它（而不是只靠 [`PartRepo::update_part_rollup`] 的终态守卫）：
    /// 终态守卫会把整段 part 派生短路成 `NoChange`，连带 step 3 的父装配件同步
    /// 也不跑 —— 于是「子件被取消、父件还停在 IN_PROCESS」这种漂移会一直留着
    /// （改造前的 `cancel` 正是这个 bug）。本策略把「跳过 part 写」与「继续派生
    /// 父层」拆开表达，两个需求互不干扰。
    KeepPartTerminalAsIs,
}

/// 批量模式的一次性意图包（参数成组演进，且已超 clippy 的 7 参阈值）。
#[derive(Debug, Clone)]
pub struct BulkStatusChange<'a> {
    /// 目标 part id（级联范围 = 该 part 下的全部活跃批次）。
    pub part_id: i64,
    /// 批次目标状态。
    pub new_status: &'a str,
    /// **反向**白名单（终态保护）：不在此列的批次才被改写。
    pub excluded_statuses: &'a [&'a str],
    /// `Some(b)` = 同时写返修标记。批量推入终态时必须给 `Some(false)`，
    /// 否则会留下 `status='CANCELLED' AND is_repairing=true` 的矛盾行
    /// （2026-10-01 review 第 1 轮 m10）。
    pub is_repairing: Option<bool>,
    /// 审计列 `updated_by`。
    pub updated_by: i64,
    /// 对 `t_part.status` 的处置（见 [`PartDerivation`]）。
    pub derivation: PartDerivation,
    /// 终态序列号归档事件的主键（见 [`StatusChange::event_id`]）。
    pub event_id: Option<i64>,
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
    /// 派生写被「终态守卫」拦下时的诊断信息（`None` = 未被拦）。
    ///
    /// 之前这条路径只留一条
    /// `tracing::warn!`：对调用方而言「part 已终态、派生被跳过」与「数据本来就
    /// 一致」都是 `NoChange`，**完全无法区分** —— admin 对账端点会因此对
    /// 「终态但错的 part」报出一份「已覆盖全表、0 变化」的假干净报告；而
    /// 终态守卫对「用 `part_ids` 定点排查」这条路径同样生效，所以定点排查
    /// 物理上不可能把那类行修好。本字段把「被拦 + 拦前
    /// 状态 + 派生值」上抛，让调用方能显式计数与逐条上报。
    pub terminal_skip: Option<TerminalSkip>,
}

/// 终态守卫命中时的诊断信息（见 [`RollupOutcome::terminal_skip`]）。
#[derive(Debug, Clone)]
pub struct TerminalSkip {
    /// `t_part.status` 的**当前**值（= 保持不变的值，必为 COMPLETED / CANCELLED）。
    pub current: String,
    /// min-progress **派生**出的值（因终态守卫未被写入）。
    pub derived: String,
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
    let outcome = rollup_part_derived(conn, part_id, ch.updated_by, ch.event_id).await?;
    Ok(outcome)
}

/// step 1：UPDATE `t_part_batch` 单行，返回该行所属 `part_id`。
///
/// 0 行 → `VERSION_CONFLICT`（批次已软删 / version 变了 / 源状态不在白名单）。
///
/// 用 `sqlx::query`（运行时）而非 `query!`（编译期）的原因：这条 SQL 的
/// SET 子句与 WHERE 守卫都随 `StatusChange` 的 `Option` 动态变化
/// （6 个可选列、可选 OCC、`= ANY($n)` 数组白名单），编译期宏无法定型。
/// 运行时 query 是本仓既有做法（见 `phase1/mod.rs::mark_batch_with_status_and_meta`）。
/// 单行写的 SQL。占位符编号与下方 `.bind()` 顺序**成对**。
///
/// 抽成 const 只为一件事：让 `bind_placeholders_are_contiguous` 单测能断言
/// 「SQL 里出现的最大占位符号 == bind 个数且 1..=N 无空洞」。2026-10-01
/// review 第 1 轮就在这里踩过一次：占位符写到 `$15` 而只 bind 了 14 个，
/// PG 在 Bind 阶段报
/// `bind message supplies 14 parameters, but prepared statement "sqlx_s_7" requires 15`，
/// 且 sqlx 的 statement cache 被污染 —— 同一连接上后续**所有**查询跟着一起炸，
/// 报错点离真凶十万八千里。
const BATCH_STATUS_UPDATE_SQL: &str = r#"
        UPDATE t_part_batch
           SET status                  = $2::varchar,
               location                = CASE WHEN $11::bool THEN NULL
                                           ELSE COALESCE($3::varchar, location) END,
               current_holder_id       = CASE WHEN $12::bool THEN NULL
                                           ELSE COALESCE($4::bigint, current_holder_id) END,
               current_process_id      = CASE WHEN $13::bool THEN NULL
                                           ELSE COALESCE($5::bigint, current_process_id) END,
               current_process_step_id = CASE WHEN $14::bool THEN NULL
                                           ELSE COALESCE($6::bigint, current_process_step_id) END,
               is_repairing            = COALESCE($7::boolean, is_repairing),
               version                 = version + 1,
               updated_at              = now(),
               updated_by              = $8::bigint
         WHERE id = $1::bigint
           AND deleted_at IS NULL
           AND status = ANY($9::varchar[])
           AND ($10::int IS NULL OR version = $10::int)
        RETURNING part_id
        "#;

/// 与 [`BATCH_STATUS_UPDATE_SQL`] 的占位符个数严格相等（= 下方 `.bind()` 的个数）。
/// 只被 `#[cfg(test)]` 的 `bind_placeholders_are_contiguous` 读。
#[cfg_attr(not(test), allow(dead_code))]
const BATCH_STATUS_UPDATE_BIND_COUNT: usize = 14;

pub(crate) async fn write_batch_status_row(
    conn: &mut PgConnection,
    ch: &StatusChange<'_>,
) -> Result<i64, AppError> {
    if ch.allowed_from.is_empty() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "批次 {} 的 allowed_from 为空，拒绝无条件改写状态",
                ch.batch_id
            ),
        ));
    }
    let row: Option<(i64,)> = sqlx::query_as(BATCH_STATUS_UPDATE_SQL)
        .bind(ch.batch_id) // $1
        .bind(ch.new_status) // $2
        .bind(ch.new_location) // $3
        .bind(ch.new_holder_id) // $4
        .bind(ch.new_process_id) // $5
        .bind(ch.new_process_step_id) // $6
        .bind(ch.is_repairing) // $7
        .bind(ch.updated_by) // $8
        .bind(ch.allowed_from) // $9
        .bind(ch.expected_version) // $10
        .bind(ch.clear_location) // $11
        .bind(ch.clear_holder_id) // $12
        .bind(ch.clear_process_id) // $13
        .bind(ch.clear_process_step_id) // $14
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
/// `PartService::sync_from_batch_change` 与本模块共用本函数，两者行为
/// 完全一致（前者是「只做派生」的历史入口，后者是「写 + 派生」的合并入口）。
///
/// ## `event_id`（2026-10-01 review 第 1 轮 M4）
///
/// `Some(id)` = 终态序列号释放时用该雪花 id 写 `SERIAL_RELEASED` 归档事件。
/// `None` = 调用方拿不到雪花生成器，此时**只清序列号、不写归档事件**并打
/// `error!` —— 理由是「宁可少一条审计行，也不能塞一个假 id」：曾经用
/// `part_id` 顶替，结果是归档事件在 `ORDER BY id DESC` 的时间线上被排到最底部，
/// 且 part 二次进终态时直接 pkey 冲突、整个事务 500。
/// 能让 part **新进**终态的写点全部传 `Some(snowflake.next_id())`；纯重算 /
/// 换 holder 类的 `sync_from_batch_change` 调用点传 `None`（它们的批次一定还
/// 处在非终态，min-progress 推不出终态，该分支不可达）。
pub async fn rollup_part_derived(
    conn: &mut PgConnection,
    part_id: i64,
    updated_by: i64,
    event_id: Option<i64>,
) -> Result<RollupOutcome, AppError> {
    // ---- step 2.1：拉 part 全部活跃批次（rollup 只看活跃行）----
    let batches = list_active_batches_by_part_id(&mut *conn, part_id).await?;
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
            terminal_skip: None,
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
            terminal_skip: None,
        });
    }

    // ---- step 2.3：派生写 `t_part`（**不走 OCC**：派生写，由行锁串行化；
    //      `version += 1` 仍写以保证审计字段单调）----
    //
    // 2026-10-01 review 第 1 轮 B1：SQL 里带**终态守卫**
    // （`status NOT IN ('COMPLETED','CANCELLED')`，见 `PartRepo::update_part_rollup`）。
    // 守卫命中 0 行有两种原因，处理方式不同：
    // - part 已是终态 → **派生层不得覆盖主操作**。`PartService::cancel` 先把
    //   part 打成 CANCELLED（主操作），紧接着的批次级联按 min-progress 会算出
    //   COMPLETED（「已完成批次 + 其余被批量取消」时 non_terminal 为空），若无守卫
    //   就会把用户的「作废工单」静默改回 COMPLETED、连带把父装配件也推成
    //   COMPLETED，而接口仍返回 200、事件流水记的是 INSPECTION→CANCELLED。
    // - part 被并发软删 → 原有防御路径。
    // 两种都降级为 `NoChange`（派生层不否决、不报错），区别只在可观测性。
    let affected = PartRepo::update_part_rollup(
        &mut *conn,
        part_id,
        &target.status,
        derived_next_process_id,
        updated_by,
    )
    .await?;
    if affected == 0 {
        // 2026-10-01 review 第 2 轮 MAJOR-1：终态守卫命中**必须上抛**诊断信息。
        // 降级语义不变（`NoChange`，派生层不否决主操作），但此前只留一条
        // `tracing::warn!`，调用方无法把它与「数据本来就一致」区分开 ——
        // admin 对账端点因此会对「终态但错的 part」报出假干净报告。
        let terminal_skip = if is_terminal(&cur.status) {
            tracing::warn!(
                part_id,
                current = %cur.status,
                target = %target.status,
                "part 派生写被终态守卫拦下：派生层不覆盖主操作写下的终态（cancel 路径）"
            );
            Some(TerminalSkip {
                current: cur.status.clone(),
                derived: target.status.clone(),
            })
        } else {
            None
        };
        // 防御：part 在两次 select 之间被并发软删。返回 NoChange 让 caller
        // 不重试（与改造前一致）。
        return Ok(RollupOutcome {
            sync: SyncOutcome::NoChange,
            part_status_changed: false,
            terminal_skip,
        });
    }

    let part_status_changed = cur.status != target.status;

    // ---- step 3：part.status 真变了 → assembly 反向同步 ----
    //
    // ⚠️ 2026-10-01 review 第 1 轮 m8：`part_status_changed == false`（只物化了
    // `next_process_id`）时**必须**回 `NoChange`。旧实现回 `Changed(part_id)`，
    // 而 `inspection_core.rs` / `worker_scan.rs` 把它当 `assembly_id` 塞进
    // 响应的 `synced_assembly_id` 并据此广播 `ASSEMBLY_UPDATED` —— WS payload
    // 里会出现一个 part id 冒充 assembly_id。
    let sync = if part_status_changed {
        AssemblyService::sync_from_part_change_by_id(conn, part_id, updated_by).await?
    } else {
        SyncOutcome::NoChange
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
    // 保证「每个 part 最多 1 条 SERIAL_RELEASED」（step 2.3 的终态守卫保证
    // part 不会再被派生推出终态，故该不变量在库层面成立）。
    if part_status_changed && is_terminal(&target.status) {
        release_part_serial_no(conn, part_id, &target.status, updated_by, event_id).await?;
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
        terminal_skip: None,
    })
}

/// step 3 + step 5：只派生**父装配件**（`t_part` 一律不碰）。
///
/// 2026-10-01 review 第 1 轮 B1 新增。给「part 已被主操作打成终态、派生层不得
/// 覆盖」的场景用（`PartDerivation::KeepPartTerminalAsIs`）：批次照常级联写，
/// 但 part 的终态一个字都不动，同时**继续**把父装配件追平 —— 否则
/// 「子件作废、父件还停在 IN_PROCESS」这种漂移会一直留着（改造前的 cancel
/// 正是如此：它压根不调任何 sync）。
pub(crate) async fn rollup_assembly_derived(
    conn: &mut PgConnection,
    part_id: i64,
    updated_by: i64,
) -> Result<SyncOutcome, AppError> {
    let sync = AssemblyService::sync_from_part_change_by_id(conn, part_id, updated_by).await?;
    if let SyncOutcome::Changed(assembly_id) = sync {
        clear_assembly_serial_no_if_terminal(conn, assembly_id, updated_by).await?;
    }
    Ok(sync)
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
///
/// `is_repairing`：2026-10-01 review 第 1 轮 m10 —— 批量推入终态时**必须**同时
/// 清返修标记，否则会留下 `status='CANCELLED'/'COMPLETED' AND is_repairing=true`
/// 的自相矛盾行（终态批次不可能还在返修）。
pub async fn apply_bulk_batch_status_change_for_part(
    conn: &mut PgConnection,
    ch: BulkStatusChange<'_>,
) -> Result<BulkSyncOutcome, AppError> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        r#"
        UPDATE t_part_batch
           SET status       = $2::varchar,
               is_repairing = COALESCE($5::boolean, is_repairing),
               version      = version + 1,
               updated_at   = now(),
               updated_by   = $3::bigint
         WHERE part_id = $1::bigint
           AND deleted_at IS NULL
           AND NOT (status = ANY($4::varchar[]))
        RETURNING part_id
        "#,
    )
    .bind(ch.part_id)
    .bind(ch.new_status)
    .bind(ch.updated_by)
    .bind(ch.excluded_statuses)
    .bind(ch.is_repairing)
    .fetch_all(&mut *conn)
    .await?;

    let affected_rows = rows.len() as u64;
    let mut part_ids: Vec<i64> = rows.into_iter().map(|(pid,)| pid).collect();
    part_ids.sort_unstable();
    part_ids.dedup();

    // TODO(2026-10-01 review 第 2 轮 MINOR-3，follow-up PR)：父装配件的派生只覆盖
    // `RETURNING` 回来的 part_id。UPDATE 命中 0 行时（part 下无可覆盖的活跃批次
    // —— 新建工单未拆批、或全部批次已在 `excluded_statuses` 里）循环体一次都不跑，
    // 父装配件**不派生**。对 force-complete（逃生通道）这没问题：part 必然已被
    // 强推；对 cancel，part 刚被主操作打成终态，父件的追平也由
    // `KeepPartTerminalAsIs` 分支承担。真正的缺口是「0 行但父装配件该追平」的其它
    // 组合（当前无此 caller）。修法：0 行时按 `ch.part_id` 兜底派生一次父件，
    // 或在 `BulkStatusChange` 上让 caller 显式给出「必须派生的 part_id」。

    for pid in &part_ids {
        match ch.derivation {
            PartDerivation::Rollup => {
                rollup_part_derived(&mut *conn, *pid, ch.updated_by, ch.event_id).await?;
            }
            PartDerivation::KeepPartTerminalAsIs => {
                // 2026-10-01 review 第 1 轮 B1：part 的终态由主操作写下，
                // 派生层只负责把**父装配件**追平（详见 `PartDerivation`）。
                //
                // TODO(2026-10-01 review 第 2 轮 MINOR-2，follow-up PR)：本分支
                // **无条件**跳过 part 写，既不校验该 part 是否真为终态、也不 warn。
                // 现状之所以安全，纯粹是因为唯一 caller
                // （`cancel_all_active_batches_for_part`）保证 part 刚被
                // `mark_part_cancelled` 打成 CANCELLED —— 这是一条**调用顺序**
                // 不变式，无类型 / 无 SQL 约束。将来误选本策略到非终态 part 上，
                // 会让该 part 的派生被无声跳过（业务流与库值长期不一致，且无任何
                // 日志）。修法：分支内先 `get_part_rollup_state` 读一次，非终态则
                // `tracing::warn!` 并降级为 `PartDerivation::Rollup` 的行为
                // （或直接返回错误，因为这是**编程错误**而非并发冲突）。
                rollup_assembly_derived(&mut *conn, *pid, ch.updated_by).await?;
            }
        }
    }

    Ok(BulkSyncOutcome {
        affected_rows,
        part_ids,
    })
}

/// 装配件级强制完成：**把该装配件全部子件的非 CANCELLED 批次强推 COMPLETED**。
///
/// 2026-10-11 新增，唯一 caller 是 `AssemblyService::force_complete`
/// （`POST /api/v2/prod/assemblies/{assembly_id}/force-complete`）。
///
/// ## 与 [`apply_bulk_batch_status_change_for_part`] 的两点差异
///
/// 1. **入口是装配件**：谓词从 `part_id = $1` 换成 `part_id IN (SELECT id FROM
///    t_part WHERE assembly_id = $1)`，一条语句覆盖全部子件的批次。
/// 2. **不派生**：本函数只执行这条 UPDATE，**不调** [`rollup_part_derived`] /
///    [`rollup_assembly_derived`]。这不是遗漏而是逃生通道的必需语义 ——
///    `apply_bulk_batch_status_change_for_part` 末尾那条 TODO 已写明「批量 UPDATE
///    命中 0 行时派生循环一次都不跑，父装配件不派生」。装配件级若沿用派生路径，
///    「子件的非取消批次恰好为 0 条」（新建未拆批 / 批次已全部 CANCELLED）时循环体
///    一次都不进，子件与父装配件都留停在原状态、端点返回 200 却什么都没改。
///    而本端点的语义是「装配件整体判为已交」，终态必须**显式写**：子件终态由
///    `AssemblyRepo::force_complete_children` 写、装配件终态由
///    `AssemblyRepo::force_complete_status` 写，两者都覆盖「批次 0 条」的情形。
///
/// 因此本函数的返回值只是诊断信息（影响行数），调用方**不得**据此判断子件是否已
/// 追平 —— 子件 / 装配件的终态一律以后续两条显式写为准。
///
/// 不走 OCC：force-complete 是逃生通道，串行化由 SQL 行锁承担。
///
/// `$N` 占位符必须连续且与 `.bind()` 个数一致（见本文件 `bind_guard_tests`）。
pub async fn force_complete_all_batches_for_assembly(
    conn: &mut PgConnection,
    assembly_id: i64,
    updated_by: i64,
) -> Result<u64, AppError> {
    // $1 assembly_id / $2 new_status / $3 updated_by / $4 excluded_statuses
    let res = sqlx::query(
        r#"
        UPDATE t_part_batch
           SET status       = $2::varchar,
               is_repairing = false,
               version      = version + 1,
               updated_at   = now(),
               updated_by   = $3::bigint
         WHERE part_id IN (
                   SELECT id
                     FROM t_part
                    WHERE assembly_id = $1::bigint
                      AND deleted_at IS NULL
               )
           AND deleted_at IS NULL
           AND NOT (status = ANY($4::varchar[]))
        "#,
    )
    .bind(assembly_id)
    .bind("COMPLETED")
    .bind(updated_by)
    .bind(vec!["CANCELLED".to_string()])
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected())
}

// ---------- step 4 / step 5：终态序列号释放 ----------

/// 子件（`t_part`）终态序列号释放：**先归档后清**。
///
/// 归档而非直接清的理由：序列号转交送货单后要从工单上消失，直接清会丢失
/// 「这个工单曾经用过哪个序列号」这条审计链。`t_part_event` 正是为此存在。
///
/// ## 事件 id（2026-10-01 review 第 1 轮 M4 修正）
///
/// 由 caller 经 [`StatusChange::event_id`] / [`BulkStatusChange::event_id`] 传一个
/// **真实雪花 id**。改造前直接拿 `part_id` 当事件 id，有两个硬伤：
/// 1. `GET /parts/{id}/events` 是 `ORDER BY id DESC`，而 `part_id` 是**建单时**的
///    雪花，比该 part 后续所有事件小若干个数量级 → 归档事件被排到时间线
///    **最底部**，看起来像建单时就发生过，而不是释放发生的那一刻；
/// 2. part 离开终态再回来时（数据修复 / admin 干预）第二次插入就是 pkey 冲突，
///    整个事务 500。
///
/// `event_id = None`（caller 拿不到生成器）时**只清序列号、不写归档**并打
/// `error!`：序列号泄漏是硬故障（`uk_t_part_serial_no` 永久占位），少一条审计行
/// 只是可观测性缺口，两害相权取轻。
///
/// ## 为什么不用「让 DB 自己发 id」（2026-10-01 review 第 1 轮 m1 订正）
///
/// `t_part_event.id` 其实**有** `DEFAULT nextval('t_part_event_id_seq')`
/// （baseline 已声明），所以「SQL 里发不出 id」这个常见理由不成立 —— 本轮
/// migration 007 的注释就是这么写的（结论仍可用，理由错）。仍然不采用
/// 「省略 id 让序列接管」的两条硬理由：
/// 1. **排序**：`GET /parts/{id}/events` 是 `ORDER BY id DESC`，序列值（1, 2, 3…）
///    会被排到全部雪花事件**最底部**，等于把归档事件藏起来；改读侧排序为
///    `created_at` 又会让 id 空间异质（同毫秒内序列 id 与雪花 id 混排）。
/// 2. **冲突**：该序列在生产里 `is_called = false`（Python 端一直显式传雪花），
///    首个 `nextval` 返回 1；若历史数据里存在序列期的小 id，就是一次 pkey 冲突
///    → 整个事务 500，而**消除它需要一个新 migration**（`setval`）。
///
/// 2026-10-11：可见性放开到 `pub(crate)`。新增 caller 是
/// `AssemblyService::force_complete` —— 装配件级强制完成后要对**被改动的子件**
/// 逐个做同样的「归档后清」，与本模块派生链共用同一条实现，避免第二条 SQL
/// 分叉出与这里不一致的归档语义（清列谓词 / 事件 id 口径）。
pub(crate) async fn release_part_serial_no(
    conn: &mut PgConnection,
    part_id: i64,
    new_status: &str,
    updated_by: i64,
    event_id: Option<i64>,
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
    match event_id {
        Some(id) => {
            PartRepo::insert_part_event(
                &mut *conn,
                NewPartEvent {
                    id,
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
        }
        None => {
            tracing::error!(
                part_id,
                new_status,
                "终态序列号已释放但未写 SERIAL_RELEASED 归档事件：调用方未提供 event_id（M4）"
            );
        }
    }

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
/// 2026-10-01 的写入口收口把写与派生焊进 [`apply_batch_status_change`]
/// 之后，「漏调」这个选项
/// 从类型层面消失了；但**绕过**本模块直接写一行的能力还在（任何人拿
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
/// - 它也正好住在被保护的那扇门（`status.rs`）里，规则的 rationale 与规则本身
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
///   10 处，分布在 queue 抢占 / move 归还 / 批次挂送货单 / 拆批扣量 /
///   `mark_batch_returned` 归还货架 / part 发料台定位）——它们命中条件 1 但不命中
///   条件 2，不该被拦。
///
/// ============================================================================
/// 另一条 CI 闸门：单行 UPDATE 的占位符 ↔ bind 个数必须一致
/// ============================================================================
///
/// 见 [`BATCH_STATUS_UPDATE_BIND_COUNT`] 的 rationale（review 第 1 轮踩过的坑）。
///
/// TODO(2026-10-01 review 第 2 轮 NIT-3，follow-up PR)：覆盖面目前**只有**
/// `BATCH_STATUS_UPDATE_SQL` 这一条单行 UPDATE。bulk 入口
/// （`apply_bulk_batch_status_change_for_part` 里那条内联 `UPDATE … RETURNING`，
/// `$1..$5`）与新增的 `t_assembly` 状态写 SQL（`AssemblyRepo::
/// update_status_if_not_terminal` / `AssemblyRepo::cancel`）**都没有**纳入。
/// 它们同样是「占位符编号 ↔ bind 顺序」的手工对齐点，写错时报的是
/// `bind message supplies N parameters …` 这种与业务毫无关系的 PG 错误，
/// 定位成本高。修法：把两条 SQL 也提成 const（与 `BATCH_STATUS_UPDATE_SQL`
/// 同款），在本 mod 内对每条跑同一个 `max_placeholder == bind 数` +
/// `1..=N 无跳号` 的断言。
#[cfg(test)]
mod bind_guard_tests {
    use super::{BATCH_STATUS_UPDATE_BIND_COUNT, BATCH_STATUS_UPDATE_SQL};

    /// SQL 里出现的最大 `$n`。
    fn max_placeholder(sql: &str) -> usize {
        let bytes = sql.as_bytes();
        let mut max = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                let mut j = i + 1;
                let mut n = 0usize;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    n = n * 10 + (bytes[j] - b'0') as usize;
                    j += 1;
                }
                max = max.max(n);
                i = j;
                continue;
            }
            i += 1;
        }
        max
    }

    #[test]
    fn bind_placeholders_are_contiguous() {
        let max = max_placeholder(BATCH_STATUS_UPDATE_SQL);
        assert_eq!(
            max, BATCH_STATUS_UPDATE_BIND_COUNT,
            "本模块单行 UPDATE：SQL 最大占位符 ${max}，但 bind 了 \
             {BATCH_STATUS_UPDATE_BIND_COUNT} 个 —— PG 会在 Bind 阶段报 \
             `bind message supplies N parameters, but prepared statement requires M`，\
             且 sqlx 的 statement cache 被污染，同连接后续所有查询一起失败。\
             请同步改 SQL 占位符与 `.bind()` 链（每个 `.bind()` 后已标注它对应哪个 $n）。"
        );
        // 1..=N 每个占位符都必须真的被用到（避免「跳号」导致 bind 顺序错位）
        for n in 1..=max {
            let needle = format!("${n}::");
            assert!(
                BATCH_STATUS_UPDATE_SQL.contains(&needle),
                "占位符 ${n} 在 SQL 里没有以 `{needle}` 形式出现（跳号会让 bind 顺序错位）"
            );
        }
    }
}

#[cfg(test)]
mod write_guard_tests {
    use std::path::{Path, PathBuf};

    /// 全仓唯一被允许写 `t_part_batch.status` 的文件（相对 crate 根）。
    ///
    /// 写死成字面量而不是 `file!()`：`file!()` 只能证明「本文件自己干净」，
    /// 而这条规则要表达的是「**别的**文件不许写」，两者不是一回事。
    const SANCTIONED: &str = "src/shared/batch/status.rs";

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
    ///     3 处、`repo/sql.rs` 3 处、`queue/repo/sql.rs` 1 处），它们是
    ///     **文档**，排除掉之后规则才能盯住真实 SQL。
    ///
    /// (b) **`#[cfg(test)]` 块** —— 排除。全仓唯一的真实例子是
    ///     `prod::queue` 的 `dispatch_batch_concurrent_modification_collects_invalid_status_failure`
    ///     里的 `UPDATE t_part_batch SET status='IN_PROCESS', version=99`：
    ///     它故意把 version 顶到 99 来**伪造一次并发改动**。`StatusChange` 的
    ///     OCC 只会 `version + 1`，表达不了「凭空跳到 99」，所以这条 fixture
    ///     无论怎么改都过不了 gate；硬要它走生产 API 只会把测试写得比生产代码
    ///     还绕。护栏要防的是**生产写路径**漏派生，不是禁止单测造数据。
    ///
    /// (c) **只读语句 / 只改其它列的 UPDATE** —— 排除，规则只看 SET 子句。
    ///
    /// ## 已知绕过口（2026-10-01 review 第 1 轮 m3 记录在案，尚未修）
    ///
    /// 1. `UPDATE ONLY t_part_batch SET status …` / `UPDATE public.t_part_batch SET …`
    ///    —— 规则要求「`update` 与表名之间只有空白」，这两种写法匹配不上；
    /// 2. `INSERT … ON CONFLICT … DO UPDATE SET status = …` 的 upsert；
    /// 3. **覆盖面只有 batch 层**：`t_part.status` / `t_assembly.status` 没有同类
    ///    护栏，而 B1 那个 bug 恰恰就发生在 part 层。
    ///
    ///    TODO(2026-10-01 review 第 2 轮，follow-up PR)：给「文件 × 表」配一张
    ///    白名单，把规则扩到 `t_part` / `t_assembly`（合法写点已知且有限：
    ///    `mark_part_cancelled` / `PartRepo::update_part_rollup` /
    ///    `AssemblyRepo::update_status_if_not_terminal` / `AssemblyRepo::cancel`）。
    ///    现状的风险是**双向**的：part / assembly 层的派生写既可能漏（无人调
    ///    sync，漂移长期留着），也可能越界（派生层覆盖主操作写下的终态 = B1）。
    ///
    /// 修 1/2 是探测器的小改（多认两个关键字），修 3 需要白名单，两者都会让本测试
    /// 的误报面显著变大，宜单独一轮改动 + 全量跑测试确认，不适合混在「修 review」
    /// 这一轮里做。
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
            "以下 {} 处直接写了 `t_part_batch.status`，绕过了 `t_part_batch` 域唯一 \
             状态写入口 `src/shared/batch/status.rs`：\n{}\n\
             \n\
             规则（见 `status.rs` 末尾 `mod write_guard_tests`）：\n\
             \x20 * 判定 = 同一语句里既有对批次表的 UPDATE、其 SET 子句又对 `status` 列赋值；\n\
             \x20 * 注释 / `#[cfg(test)]` 块内、以及只改其它列的 UPDATE 不在判定范围内。\n\
             \n\
             正确写法：改用 `shared::batch::status::apply_batch_status_change`（单行）或\n\
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
