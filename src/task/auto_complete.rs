//! 后台定时任务：自动 DELIVERED → COMPLETED
//!
//! 对应 Python myERP/service/auto_complete.py 与 `core/database.py::lifespan`：
//! - 启动后立即跑一轮
//! - 间隔 `AUTO_COMPLETE_INTERVAL_HOURS`（默认 24h）
//! - 扫描 DELIVERED 且 `placed_at < now() - interval 'AUTO_COMPLETE_THRESHOLD_DAYS days'`
//!   的批次，逐个调用 `PartService::complete`（DELIVERED → COMPLETED）。
//! - 收到 `CancellationToken` 时优雅退出
//!
//! Phase 0 实现要点：
//! - 单事务包住「扫描 + 逐个 complete」：commit 后再广播 WS 事件（CLAUDE.md §架构 6）。
//! - 构造伪 `CurrentUser`（id=0, username="system"）承担后台身份——
//!   `PartService::complete` 要求 MANAGER/CLERK 角色；后续接 RBAC 时再升级。
//! - 单批失败 log 继续（与 Python `_run_once` 一致），不影响后续批次。
//! - 本 PR **不**做 latest-event-derived 阈值（Python `_run_once` 走
//!   `t_part_event` 关联拿 `latest_delivered`，避免 `placed_at` 与 DELIVERED
//!   时间不对齐的偏差），仅按 `placed_at` 直接过滤——`placed_at` 是批次首次
//!   ON_SHELF 时间，简化版可接受；如需严格按事件时间迁移再升级。

use std::sync::Arc;
use std::time::Duration;

use chrono::Duration as ChronoDuration;
use sqlx::PgConnection;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::dto_crud::CompleteRequest;
use crate::modules::part::service::PartService;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::shared::error::AppError;
use crate::state::AppState;

/// 后台任务入口（main.rs 中 `tokio::spawn`）
pub async fn run(state: Arc<AppState>, token: CancellationToken) {
    let cfg = state.config.auto_complete;
    let mut ticker = interval(Duration::from_secs(cfg.interval_hours.max(1) * 3600));

    info!(
        threshold_days = cfg.threshold_days,
        interval_hours = cfg.interval_hours,
        "auto_complete 任务已启动"
    );

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = run_once(&state, cfg.threshold_days).await {
                    warn!(error = %e, "auto_complete 一轮失败");
                }
            }
            _ = token.cancelled() => {
                info!("auto_complete 任务收到取消信号，退出");
                break;
            }
        }
    }
}

/// 一轮扫描 + 处理：
/// 1. 算阈值 `threshold = now_naive() - threshold_days`；
/// 2. 开事务；
/// 3. `PartBatchRepo::find_delivered_older_than(&mut *tx, threshold)` 拿候选
///    `(batch_id, part_id, version)`；
/// 4. 对每行调 `PartService::complete`（用伪 system CurrentUser）；
/// 5. commit；
/// 6. commit 成功后批量 `ws_hub.broadcast` 每行的 PART_COMPLETED 事件。
///
/// 单批失败 log 继续；commit 包含所有已成功的批（失败批不影响事务，因
/// `PartService::complete` 内部用 OCC + status guard，失败会直接返回 Err）。
///
/// 注意：`PartService::complete` 在第 4 步可能 throw `VERSION_CONFLICT`
/// （行被并发改了）或 `BIZ_PART_NOT_DELIVERED`（状态已变）。两都视为「跳过」
/// 即可，不影响后续批次。
pub async fn run_once(state: &Arc<AppState>, threshold_days: u32) -> anyhow::Result<()> {
    let threshold = now_naive() - ChronoDuration::days(threshold_days as i64);

    let system_user = system_current_user();
    let pool = &state.pool;

    // 1. 开事务
    let mut tx = pool.begin().await?;

    // 2. 扫描候选
    let candidates = PartBatchRepo::find_delivered_older_than(&mut *tx, threshold).await?;
    if candidates.is_empty() {
        info!(threshold = %threshold, "auto_complete: no batches to complete");
        // 早 commit 避免留长事务
        tx.commit().await?;
        return Ok(());
    }
    info!(
        count = candidates.len(),
        threshold = %threshold,
        "auto_complete: scanning DELIVERED batches"
    );

    // 3. 逐个 complete；失败的 log + 跳过
    let mut completed: Vec<(i64, i64)> = Vec::with_capacity(candidates.len());
    for (batch_id, part_id, version) in candidates {
        let req = CompleteRequest {
            batch_id,
            version,
            note: Some("auto_complete".to_string()),
        };
        match complete_one(&mut tx, &state.snowflake, part_id, req, &system_user).await {
            Ok(_) => {
                completed.push((batch_id, part_id));
                info!(
                    batch_id,
                    part_id, "auto_complete: completed batch"
                );
            }
            Err(e) => {
                warn!(
                    batch_id,
                    part_id,
                    error = %e,
                    "auto_complete: failed to complete batch (skipped)"
                );
            }
        }
    }

    // 4. commit（一次性包含所有成功的批）
    tx.commit().await?;
    info!(
        completed_count = completed.len(),
        "auto_complete: round finished"
    );

    // 5. commit 后才广播（CLAUDE.md §架构约定 6）
    for (batch_id, part_id) in &completed {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "PART_COMPLETED".to_string(),
            payload: serde_json::json!({
                "batch_id": batch_id.to_string(),
                "part_id": part_id.to_string(),
                "source": "auto_complete",
            }),
        });
    }

    Ok(())
}

/// 构造后台伪 CurrentUser：id=0 / username="system" / MANAGER 角色。
///
/// `PartService::complete` 要求 `Manager` / `Clerk` 角色（lifecycle.rs:249）。
/// 系统身份用 MANAGER——与 Python `service/auto_complete.py` 等价（Python
/// 不走 service 的角色守卫，后台循环直接调 service）。
pub(crate) fn system_current_user() -> CurrentUser {
    CurrentUser {
        id: 0,
        username: "system".to_string(),
        roles: vec![Role::Manager],
        shelf_ids: Vec::new(),
        shelf_wildcard: true,
    }
}

/// 包装 `PartService::complete`：在事务（`&mut PgConnection`）内调用。
async fn complete_one(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    part_id: i64,
    req: CompleteRequest,
    current: &CurrentUser,
) -> Result<crate::modules::part::dto::PartOut, AppError> {
    PartService::complete(conn, snowflake, part_id, req, current).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `system_current_user` 构造合法性：id=0 / MANAGER 角色。
    /// 关键约束：`PartService::complete` 走 `require_any_role(Manager|Clerk)`，
    /// 系统身份必须满足其一；这里选 MANAGER（与 Python 等价的「最高权限」路径）。
    #[test]
    fn system_current_user_has_manager_role() {
        let u = system_current_user();
        assert_eq!(u.id, 0);
        assert_eq!(u.username, "system");
        assert!(u.has_role(Role::Manager));
        assert!(u.shelf_wildcard);
    }

    /// threshold 计算：`now_naive() - days` 必须严格早于 `now_naive()`。
    /// 验证 ChronoDuration::days 用法正确，不退化到 0。
    #[test]
    fn threshold_days_subtracts_from_now() {
        let now = now_naive();
        let t = now - ChronoDuration::days(7);
        assert!(t < now);
        // 差值约 7 天（允许 ±1s clock drift）
        let diff = (now - t).num_seconds();
        assert!(
            (7 * 86_400 - 1..=7 * 86_400 + 1).contains(&diff),
            "diff = {diff}s, expected ≈604800s"
        );
    }

    /// threshold = 0 天（caller 配错）应等于 `now`，保证 DELIVERED 批次
    /// 因 `placed_at < now` 永远命中——这不是想要的语义，但属于「caller 错」
    /// 而非库错；这里只验证算式正确。
    #[test]
    fn threshold_zero_days_equals_now() {
        let now = now_naive();
        let t = now - ChronoDuration::days(0);
        assert_eq!(t, now);
    }
}
