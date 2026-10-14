//! `serial` 域业务逻辑：释放终态工单占用的序列号（**2026-10-19 新建**）
//!
//! 自由函数（范式同 `mail_digest::service` / `statistics::service` / `print_template::service`），
//! 签名收 `&mut PgConnection` —— **service 不知事务**，事务边界在 handler
//! （CLAUDE.md「必须遵守的架构约定」第 1 条）。
//!
//! ## 这个端点解决的故障
//!
//! 序列号是**全局唯一且被 partial unique index 守着**的（`uk_t_assembly_serial_no` /
//! `uk_t_part_serial_no`，谓词都是 `serial_no IS NOT NULL`）—— 序列号一旦发出去就
//! **永久占位**，没有「回收站」。正常路径下终态工单的序列号由既有状态机派生链释放
//! （批次流转 / 强制完成都会清），但历史数据里有相当一部分**从来没走过那条链**
//! （早期直接改库的工单、外协发出去后再取消的、以及 2026-10-01 那次
//! `20261001000200_007_serial_release.sql` 迁移之前入库的行）⇒ 序列号泄漏、
//! 新工单取号被 `uk_*_serial_no` 挡住。本端点就是**一键补上那笔欠账**。
//!
//! ## ⚠️ 为什么**复用**两个既有原语，而不是新写一套 UPDATE
//!
//! 本域**一行写 SQL 都不写**（只写 SELECT）。清列动作全部委托给仓库里已有的两个原语：
//!
//! | 侧 | 原语 | 自带语义 |
//! |---|---|---|
//! | 装配件 | `AssemblyRepo::clear_serial_no_if_terminal` | 三条守卫 + `version+1`；**不写归档**（`t_assembly` 无事件表，`note` 是用户可编辑业务字段） |
//! | 零件 / 子件 | `shared::batch::status::release_part_serial_no` | **先读原值 → 写 `t_part_event`（`SERIAL_RELEASED`）→ 再清列** + `version+1` |
//!
//! 自己另写一条 `UPDATE t_part SET serial_no = NULL` 会有**三处**立刻分叉：
//! ① 子件的归档事件直接消失（审计链断）；② `version` 不递增 ⇒ 已把该 version 当
//! OCC 锚的并发请求全部失去冲突检测；③ 终态守卫口径与状态机那条链不一致 ⇒ 日后
//! 「正常流转会清、本端点不清」或反之，出现只在某条路径上复现的序列号泄漏。
//! 这与 `docs/api/batch.md` 里「写 `t_part_batch.status` 只能走
//! `apply_batch_status_change`」是**同一条纪律**。
//!
//! ## ⚠️ 并发口径：候选 SELECT 带 `FOR UPDATE`
//!
//! apply 路径先 `SELECT … FOR UPDATE` 把候选行**锁在事务内**，再逐行调原语。理由见
//! [`repo::list_terminal_assemblies`] 的 doc（核心：清列原语自带守卫，若不锁，
//! 「选候选」与「清列」之间状态可能被并发流转改掉，导致计数与实动行数对不上）。
//!
//! ## ⚠️ 雪花 event_id：**每个 part 各拿一个独立 id**
//!
//! `t_part_event.id` 是 pkey。`release_part_serial_no` 的 `event_id` 形参**必须**传
//! `Some(真实雪花 id)`：传 `None` 会走「只清不归档 + `tracing::error!`」的降级分支，
//! 那条分支是为了「caller 拿不到生成器」的兜底，本域明明有 `state.snowflake` 却降级
//! 是白白丢掉审计行。**更不能复用同一个 id**：本端点一次会清几百上千个 part，
//! 复用同一个 id 必然撞 pkey（23505）把**整个事务**回滚 ⇒ 一行都清不掉。
//!
//! ## 计数口径：`assembly_count` / `part_count` 不是 `affected.len()`
//!
//! `affected` 受 `dto::RELEASE_DETAIL_LIMIT` 截断，用它当计数会让「实动 5000 行、
//! 报 200 行」。apply 路径的计数**取自锁内候选集合**（装配件侧再叠加原语实际返回的
//! `rows_affected`），dry_run 路径取自 `COUNT(*)` —— 两者在守卫同源 + 锁持有时恒等。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::serial::dto::{
    KIND_ASSEMBLY, KIND_PART, RELEASE_DETAIL_LIMIT, ReleaseTerminalOut, ReleaseTerminalRequest,
    ReleasedSerialItem,
};
use crate::modules::serial::repo::{self, TerminalSerialRow};
use crate::shared::error::AppError;
use sqlx::PgConnection;

/// 一键释放「已完成 / 已取消且仍占着序列号」的工单（装配件 + 零件 / 子件两张表）。
///
/// **守卫顺序**（逐条 early-fail，顺序固定）：① `require_role(Manager)`。
/// 角色守卫放在**最前** ⇒ 无权账号连候选 SELECT 都跑不到（本端点会全表扫两张表，
/// 不该让无权者拿它做信息探测）。
///
/// - `dry_run = true`：**零写入**，4 条 SELECT（2 条 count + 2 条 list），不开事务、
///   不广播 WS。
/// - `dry_run = false`：开事务 → 锁候选 → 逐行调既有释放原语 → 由 handler commit。
pub async fn release_terminal(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    req: ReleaseTerminalRequest,
    current: &CurrentUser,
) -> Result<ReleaseTerminalOut, AppError> {
    // ① 角色闸：MANAGER 单角色（逃生通道 / 数据修复，不下放 CLERK），
    //    与 `force_complete` 同口径。ADMIN 元角色由 require_role 内部短路放行。
    current.require_role(Role::Manager)?;

    let limit = Some(RELEASE_DETAIL_LIMIT as i64);

    if req.dry_run {
        // ── 预览：4 条 SELECT、零写入 ──
        let (assembly_count, part_count) = repo::count_terminal_with_serial(conn).await?;
        let assemblies = repo::list_terminal_assemblies(conn, limit, false).await?;
        let parts = repo::list_terminal_parts(conn, limit, false).await?;
        let total = assembly_count + part_count;
        return Ok(ReleaseTerminalOut {
            dry_run: true,
            assembly_count,
            part_count,
            released_total: total,
            truncated: total > RELEASE_DETAIL_LIMIT as i64,
            affected: detail_items(&assemblies, &parts),
        });
    }

    // ── 实跑 ──
    // 候选**不加 LIMIT**（`None`）：明细上限只约束「回给前端看多少行」，不约束
    // 「真正清多少行」。加了 LIMIT 就变成「5000 行残留只清得掉 200 行」。
    let assemblies = repo::list_terminal_assemblies(conn, None, true).await?;
    let parts = repo::list_terminal_parts(conn, None, true).await?;

    // 装配件侧：逐行调既有原语。它自带三条守卫并返回 rows_affected；锁已持有时
    // 正常必为 1，0 行只是防御性跳过（不 panic、不报错 —— 强行报错会把整批回滚）。
    let mut assembly_count = 0i64;
    for row in &assemblies {
        let affected_rows =
            AssemblyRepo::clear_serial_no_if_terminal(&mut *conn, row.id, current.id)
                .await
                .map_err(AppError::from)?;
        assembly_count += affected_rows as i64;
    }

    // 零件侧：逐行调「先归档后清」原语。**每行一个独立雪花 id**（见本文件模块 doc）。
    // `row.status` 作为事件的 `to_status` —— 它取自锁内 SELECT，是该行释放时刻的
    // 真实状态（COMPLETED / CANCELLED，守卫保证）。
    let mut part_count = 0i64;
    for row in &parts {
        crate::shared::batch::status::release_part_serial_no(
            &mut *conn,
            row.id,
            &row.status,
            current.id,
            Some(snowflake.next_id()),
        )
        .await?;
        part_count += 1;
    }

    let total = assembly_count + part_count;
    Ok(ReleaseTerminalOut {
        dry_run: false,
        assembly_count,
        part_count,
        released_total: total,
        truncated: total > RELEASE_DETAIL_LIMIT as i64,
        affected: detail_items(&assemblies, &parts),
    })
}

/// 把两张表的候选行拼成出参明细（**装配件在前、子件在后**，总数封顶
/// `RELEASE_DETAIL_LIMIT`）。
///
/// **为什么装配件排在前面**：子件序列号是由父装配件派生的（`{serial_no}-{i:02d}`），
/// 释放**父件**序列号的影响面更大（连带一批子件的号段一起让出来）⇒ 明细与用户的心智
/// 都应该先看装配件。代价是极端形态（装配件残留极多、子件很少）下子件会被挤出 200
/// 条上限 —— 这是有意的取舍，`released_total` / `part_count` 仍如实报出子件总数。
fn detail_items(
    assemblies: &[TerminalSerialRow],
    parts: &[TerminalSerialRow],
) -> Vec<ReleasedSerialItem> {
    assemblies
        .iter()
        .map(|r| item(KIND_ASSEMBLY, r))
        .chain(parts.iter().map(|r| item(KIND_PART, r)))
        .take(RELEASE_DETAIL_LIMIT)
        .collect()
}

/// 单行 → 出参明细（雪花 id 在这里字符串化）。
fn item(kind: &'static str, row: &TerminalSerialRow) -> ReleasedSerialItem {
    ReleasedSerialItem {
        kind,
        id: row.id.to_string(),
        serial_no: row.serial_no.clone(),
        drawing_no: row.drawing_no.clone(),
        status: row.status.clone(),
    }
}
