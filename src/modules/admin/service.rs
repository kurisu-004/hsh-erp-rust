//! admin 域业务逻辑：对账 / 修数据的**逃生口**
//!
//! 2026-10-01 新增。
//!
//! ## 这个域解决什么问题
//!
//! `t_part_batch.status` → `t_part.status` → `t_assembly.status` 是三层单向派生，
//! 写入口已收口到 `part::service::status_gate`（单测
//! `part::service::status_gate::write_guard_tests::no_outside_file_writes_batch_status`
//! 守住「只有它能写批次状态」）。但派生**缓存**仍可能与真源不一致：
//!
//! 1. **历史漂移**：status_gate 收口之前有 3 个写点漏调 sync，线上/备份库里已经存在
//!    「批次全完成、part 还停在 IN_PROCESS」这类行。代码再正确也修不了既有数据。
//! 2. **事后漂移**：极端情况下（进程在 commit 与广播之间被杀、手工 SQL 改过库、
//!    早期版本的 bug）派生缓存仍可能偏旧。正常路径下次任意 part 流转会自愈，
//!    但如果那个 part 再也不动了，漂移就永久留着。
//!
//! 所以提供 `POST /api/v2/admin/recompute-rollup`：**不新增任何派生逻辑**，
//! 只是把已有的 rollup 函数（`status_gate::rollup_part_derived` 与
//! `assembly::service::sync_from_part::sync_assembly_status`）在一个可限域、可限量、
//! 分块提交的循环里跑一遍。
//!
//! ## 为什么「复用而不是重算」是关键约束
//!
//! 派生算法（min-progress、终态序列号释放、OUTSOURCE→IN_PROCESS 映射……）是本仓
//! 最容易「两份实现漂移」的地方。若对账端点自己再写一遍 `UPDATE t_part SET status =
//! <自己算的>`，那么以后改算法就必然出现「业务流算 A、对账算 B」，对账端点会
//! 变成**新的数据损坏源**。所以这里一行算法都不写，只做「取 id → 调既有函数 →
//! 记 before/after」。
//!
//! ## 事务边界
//!
//! 分块由 handler 驱动（每块一个 `pool.begin()` / `commit()`），service 只收
//! `&mut PgConnection`、自己不开事务 —— 与全仓「事务边界在 handler」约定一致。

use sqlx::PgConnection;

use crate::modules::assembly::service::sync_from_part::recompute_assembly_status_by_id;
use crate::modules::part::repo::sql::PartRepo;
use crate::modules::part::service::status_gate;
use crate::shared::error::AppError;

use super::dto::StatusChangeEntry;

/// 全量对账时每个 id 列表的默认行数上限。
pub const DEFAULT_LIMIT: i64 = 1_000;

/// `limit` 硬上限。超过 → `20104 BIZ_INVALID_VALUE`。
///
/// 取 10_000 的理由：单次请求对 DB 的压力与行数近似线性，10_000 行派生写在
/// 分块提交下大致是秒级；再大就没必要走「全量」了，运维应改传显式 id 列表。
pub const MAX_LIMIT: i64 = 10_000;

/// 显式 id 列表的长度上限（同上：单请求影响面有界）。
pub const MAX_SCOPE_IDS: usize = 1_000;

/// 校验并归一化 `limit`：缺省 → [`DEFAULT_LIMIT`]；`≤0` / `> MAX_LIMIT` → 20104。
pub fn resolve_limit(limit: Option<i64>) -> Result<i64, AppError> {
    let v = limit.unwrap_or(DEFAULT_LIMIT);
    if v <= 0 {
        return Err(AppError::biz(
            crate::shared::error::code::BIZ_INVALID_VALUE,
            format!("limit 必须为正整数，收到 {v}"),
        ));
    }
    if v > MAX_LIMIT {
        return Err(AppError::biz(
            crate::shared::error::code::BIZ_INVALID_VALUE,
            format!("limit 上限 {MAX_LIMIT}，收到 {v}"),
        ));
    }
    Ok(v)
}

/// 校验显式 id 列表长度上限。
pub fn validate_scope_ids(kind: &str, ids: &[i64]) -> Result<(), AppError> {
    if ids.len() > MAX_SCOPE_IDS {
        return Err(AppError::biz(
            crate::shared::error::code::BIZ_INVALID_VALUE,
            format!("{kind} 最多 {MAX_SCOPE_IDS} 个，收到 {}", ids.len()),
        ));
    }
    Ok(())
}

/// 取待对账的 `t_part.id`（`limit + 1` 条，多取一条用来判断是否 `truncated`）。
///
/// 按 `id` 升序：让「限量」语义是**确定性的窗口**，而不是随机抽样，运维续扫
/// 的可预期性才够用。
///
/// `after_id` = **游标**（2026-10-01 review 第 1 轮 M7 新增）：只取 `id > after_id`
/// 的行。原实现没有游标 / offset，`truncated = true` 时运维再调一次还是从最小的
/// `limit` 行开始扫 —— 一个「兜底对账」端点在生产数据量下永远兜不住底。
/// 游标由调用方从上一轮响应的 `next_after_id` 原样回传。
pub async fn list_part_ids(
    conn: &mut PgConnection,
    limit: i64,
    after_id: Option<i64>,
) -> Result<(Vec<i64>, bool), AppError> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT id FROM t_part WHERE deleted_at IS NULL AND id > $1 ORDER BY id LIMIT $2",
    )
    .bind(after_id.unwrap_or(0))
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;
    Ok(split_truncated(
        rows.into_iter().map(|(i,)| i).collect(),
        limit,
    ))
}

/// 取待对账的 `t_assembly.id`（同 [`list_part_ids`] 的语义，含同一个游标）。
pub async fn list_assembly_ids(
    conn: &mut PgConnection,
    limit: i64,
    after_id: Option<i64>,
) -> Result<(Vec<i64>, bool), AppError> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT id FROM t_assembly WHERE deleted_at IS NULL AND id > $1 ORDER BY id LIMIT $2",
    )
    .bind(after_id.unwrap_or(0))
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;
    Ok(split_truncated(
        rows.into_iter().map(|(i,)| i).collect(),
        limit,
    ))
}

/// 本轮窗口的续扫游标（`truncated = true` 时 = 最后一个已处理的 id）。
///
/// 没有它，运维就只能靠「显式传 id 列表」续扫 —— 那要求先把全表 id 查出来
/// 再分批贴回来，一个「兜底」端点不该长这样。
pub fn next_cursor(processed_ids: &[i64]) -> Option<i64> {
    processed_ids.iter().copied().max()
}

fn split_truncated(mut ids: Vec<i64>, limit: i64) -> (Vec<i64>, bool) {
    let truncated = ids.len() as i64 > limit;
    ids.truncate(limit as usize);
    (ids, truncated)
}

/// 重算单个 part 的派生缓存（`t_part.status` + `next_process_id`）。
///
/// **完全复用** [`status_gate::rollup_part_derived`] —— 本函数只在外面包一层
/// before/after 读数，用于生成报告。part 不存在 / 已软删 → `Ok(None)`
/// （`rollup_part_derived` 自身对「无活跃批次」也是 NoChange，不报错）。
///
/// `next_process_fixed` 单独返回是因为它与 status 是**两条独立的派生列**：
/// 批次状态没变、但批次被挂到了新工序（`current_process_id` 变了）时，
/// `t_part.next_process_id` 会漂，而 status 不动。两者都会导致业务错误
/// （后者会让派工指到上一道工序），所以都要报。
///
/// `event_id`（2026-10-01 review 第 1 轮 M4）：本端点**有可能**把一个 part 从
/// 非终态推成终态（例如脏数据里批次全 COMPLETED 而 part 还停在 INSPECTION），
/// 那一步会释放并归档序列号，故 handler 必须传一个真实雪花 id。
///
/// ⚠️ 已终态（COMPLETED / CANCELLED）的 part **不参与**对账：`update_part_rollup`
/// 带终态守卫（B1），派生层不会覆盖主操作写下的终态。详见
/// `docs/api/admin.md`。
pub async fn recompute_part(
    conn: &mut PgConnection,
    part_id: i64,
    updated_by: i64,
    event_id: Option<i64>,
) -> Result<PartRecompute, AppError> {
    let before = PartRepo::get_part_rollup_state(&mut *conn, part_id).await?;
    // 返回值里的 `sync` 是给 status_gate 内部级联装配件用的，对账报告不消费
    status_gate::rollup_part_derived(conn, part_id, updated_by, event_id).await?;
    let after = PartRepo::get_part_rollup_state(&mut *conn, part_id).await?;
    let Some((before, after)) = before.zip(after) else {
        return Ok(PartRecompute::unchanged());
    };
    let status_changed = before.status != after.status;
    let next_process_fixed = before.next_process_id != after.next_process_id;
    let change = status_changed.then(|| StatusChangeEntry {
        level: "PART",
        id: part_id,
        from: before.status.clone(),
        to: after.status.clone(),
    });
    Ok(PartRecompute {
        // `outcome.sync` 已被 `rollup_part_derived` 内部用于级联装配件，这里不重复播报
        entry: change,
        next_process_fixed,
    })
}

/// 单 part 对账的结果。
#[derive(Debug, Clone, Default)]
pub struct PartRecompute {
    /// `Some` = `t_part.status` 真变了（before → after 已填好）。
    pub entry: Option<StatusChangeEntry>,
    /// `next_process_id` 派生指针是否被修正。
    pub next_process_fixed: bool,
}

impl PartRecompute {
    fn unchanged() -> Self {
        Self {
            entry: None,
            next_process_fixed: false,
        }
    }
}

/// 重算单个装配件的聚合状态（part → assembly）。
///
/// 复用 `assembly` 域**唯一**的聚合实现（`sync_assembly_status`：拉子件状态 →
/// `compute_assembly_target` → OCC + 终态守卫写回），不重写一遍。
pub async fn recompute_assembly(
    conn: &mut PgConnection,
    assembly_id: i64,
    updated_by: i64,
) -> Result<Option<StatusChangeEntry>, AppError> {
    let before: Option<(String,)> =
        sqlx::query_as("SELECT status FROM t_assembly WHERE id = $1 AND deleted_at IS NULL")
            .bind(assembly_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((before_status,)) = before else {
        // 已软删 / 不存在：对账不是业务操作，跳过而不是抛 404（批量窗口里
        // 夹一条已删行不该让整轮失败）。
        return Ok(None);
    };
    let outcome = recompute_assembly_status_by_id(conn, assembly_id, updated_by).await?;
    if !matches!(
        outcome,
        crate::modules::assembly::service::SyncOutcome::Changed(_)
    ) {
        return Ok(None);
    }
    let after: (String,) = sqlx::query_as("SELECT status FROM t_assembly WHERE id = $1")
        .bind(assembly_id)
        .fetch_one(&mut *conn)
        .await?;
    if after.0 == before_status {
        return Ok(None);
    }
    Ok(Some(StatusChangeEntry {
        level: "ASSEMBLY",
        id: assembly_id,
        from: before_status,
        to: after.0,
    }))
}
