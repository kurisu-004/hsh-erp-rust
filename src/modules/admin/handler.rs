//! admin 域 HTTP handler
//!
//! 2026-10-01 新增：`POST /api/v2/admin/recompute-rollup`。
//!
//! ## 端点
//! - `POST /api/v2/admin/recompute-rollup` —— **Manager 单角色**。按
//!   `t_part_batch` → `t_part` → `t_assembly` 重跑派生算法并回报
//!   「检查了多少 / 变了多少 / 每条 before → after」。可限域（`part_ids` /
//!   `assembly_ids`）、可限量（`limit`）、可省略 body（全量）。详见
//!   [`../service.rs`](./service.rs) 模块文档。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{RecomputeRollupReport, RecomputeRollupRequest};
use super::service::{
    PartRecompute, list_assembly_ids, list_part_ids, next_cursor, recompute_assembly,
    recompute_part, resolve_limit, validate_scope_ids,
};

/// 逐块处理的大小（**每块一个事务**）。
///
/// 200 是权衡值：太小则事务开销占比高、WS 之前的长尾拉长；太大则单事务持有的
/// `t_part` / `t_assembly` 行锁窗口变长，与线上送检 / 交付的正常写争锁。
/// 一块 200 个 part 时，块内每个 part 约 4~5 条 SQL，量级是几百毫秒。
///
/// 分块（而不是一个大事务包到底）还有两个好处：
/// 1. 某一块撞到脏数据 / 瞬时 DB 错误时，前面几块**已经提交**，重试只需重跑失败块
///    （本端点幂等，重跑无副作用）；
/// 2. 单事务覆盖全表会在 commit 前一直占着全部派生写锁，而派生写**不走 OCC**
///    （靠行锁串行化），锁窗口 = 业务侧的等待时间。
const CHUNK_SIZE: usize = 200;

/// `POST /api/v2/admin/recompute-rollup`
///
/// 权限：**Manager**（唯一角色）。理由：它是对**全量数据**动手的修数端点，
/// 一次误用就能把成千上万行的派生状态改掉；而任何一次误用都可以靠再跑一次
/// （幂等）追平，但错误权限造成的误用本身无法回滚。
///
/// 事务：分块开事务（见 `CHUNK_SIZE` 注释），每块 commit 一次；WS 广播在
/// **全部块提交之后**发一次汇总事件（`ROLLUP_RECOMPUTED`）。
///
/// body 可省略（`Option<Json<_>>`）：axum 的 `OptionalFromRequest` 在没有
/// `Content-Type` 头时返回 `None` → 语义等于全量对账。
pub async fn recompute_rollup(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    body: Option<Json<RecomputeRollupRequest>>,
) -> Result<Json<R<RecomputeRollupReport>>, AppError> {
    current.require_role(Role::Manager)?;
    let req = body.map(|Json(v)| v).unwrap_or_default();

    let limit = resolve_limit(req.limit)?;
    let part_ids = req.part_ids.filter(|v| !v.is_empty());
    let assembly_ids = req.assembly_ids.filter(|v| !v.is_empty());
    if let Some(ids) = part_ids.as_deref() {
        validate_scope_ids("part_ids", ids)?;
    }
    if let Some(ids) = assembly_ids.as_deref() {
        validate_scope_ids("assembly_ids", ids)?;
    }
    let scope = match (part_ids.is_some(), assembly_ids.is_some()) {
        (false, false) => "ALL",
        (true, false) => "PART_IDS",
        (false, true) => "ASSEMBLY_IDS",
        (true, true) => "PART_IDS+ASSEMBLY_IDS",
    }
    .to_string();
    // 只有「两个 id 列表都没给」（= 省略 body 的全量简写）才展开全表窗口。
    // 只给 `assembly_ids` 时 **part 段必须完全跳过** —— 否则「我没让你动 part」
    // 会被解读成「全量重算 part」，那是本次对账最危险的误用（无谓地重写全表
    // 派生列 + 白占行锁）。
    let full_scope = part_ids.is_none() && assembly_ids.is_none();

    let mut report = RecomputeRollupReport {
        scope,
        ..Default::default()
    };

    // ---- 解析待处理 id 集合（只读，不开事务）----
    // 「不限 id」时才需要限量窗口；显式传 id 时以调用方给的为准（已过长度校验）。
    // 读端点形态：`pool.acquire()` 不开事务（架构约定 1 的例外清单）
    let mut conn = state.pool.acquire().await?;
    let (mut part_targets, part_truncated) = match (part_ids.clone(), full_scope) {
        (Some(ids), _) => (ids, false),
        (None, true) => list_part_ids(&mut conn, limit, req.after_id).await?,
        (None, false) => (Vec::new(), false),
    };
    let (mut assembly_targets, asm_truncated) = match (assembly_ids.clone(), full_scope) {
        (Some(ids), _) => (ids, false),
        (None, true) => list_assembly_ids(&mut conn, limit, req.after_id).await?,
        (None, false) => (Vec::new(), false),
    };
    drop(conn);
    report.truncated = part_truncated || asm_truncated;
    // 2026-10-01 review 第 1 轮 M7：把续扫游标回给调用方。
    //
    // 两段窗口各自推进到哪由各自的最大 id 决定；`t_assembly` 的 id 空间与
    // `t_part` **不相交**（各自独立的雪花流），但两个游标共用一个 `after_id`
    // 字段会互相干扰 —— 故取**两段里较大的**那个作为续扫游标：它保证本轮
    // 已处理的两段都不会被下一轮重复处理（幂等重复处理只是浪费，不正确）；
    // 代价是下一轮会跳过 id 介于两者之间的少量行，运维若要严格覆盖可对两段
    // 分别用 `part_ids` / `assembly_ids` 定点跑（端点本就支持）。
    if report.truncated {
        let p = next_cursor(&part_targets);
        let a = next_cursor(&assembly_targets);
        report.next_after_id = match (p, a) {
            (Some(p), Some(a)) => Some(p.max(a)),
            (Some(p), None) | (None, Some(p)) => Some(p),
            (None, None) => None,
        };
    }

    // ---- part 段：batch → part（父装配件由 status_gate 内部级联）----
    for chunk in part_targets.chunks(CHUNK_SIZE) {
        let mut tx = state.pool.begin().await?;
        for part_id in chunk {
            let PartRecompute {
                entry,
                next_process_fixed,
            } = recompute_part(
                &mut tx,
                *part_id,
                current.id,
                Some(state.snowflake.next_id()),
            )
            .await?;
            report.parts_examined += 1;
            if next_process_fixed {
                report.parts_next_process_id_fixed += 1;
            }
            if let Some(entry) = entry {
                report.parts_changed += 1;
                report.changes.push(entry);
            }
        }
        tx.commit().await?;
    }
    part_targets.clear();

    // ---- assembly 段：part → assembly ----
    //
    // 必须排在 part 段**之后**：聚合读的是子件的**当前** status。若顺序反过来，
    // 本轮刚被修正的 part 不会被计进父件的聚合，父件会再旧一轮（要等下一次
    // 调用才对齐，破坏「调一次就收敛」的直觉）。
    for chunk in assembly_targets.chunks(CHUNK_SIZE) {
        let mut tx = state.pool.begin().await?;
        for assembly_id in chunk {
            report.assemblies_examined += 1;
            if let Some(entry) = recompute_assembly(&mut tx, *assembly_id, current.id).await? {
                report.assemblies_changed += 1;
                report.changes.push(entry);
            }
        }
        tx.commit().await?;
    }
    assembly_targets.clear();

    // 什么都没需要改 → 仍然 200（幂等是**成功**语义，不是错误语义）。
    // 前端据此把按钮置灰 / 展示「数据已一致」。
    if report.parts_changed > 0 || report.assemblies_changed > 0 {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "ROLLUP_RECOMPUTED".into(),
            payload: json!({
                "scope": report.scope,
                "parts_changed": report.parts_changed,
                "assemblies_changed": report.assemblies_changed,
                "operator_id": current.id.to_string(),
            }),
        });
    }
    Ok(Json(R::ok(report)))
}
