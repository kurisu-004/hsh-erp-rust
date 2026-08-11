//! 后台定时任务：自动 DELIVERED → COMPLETED
//!
//! 对应 Python myERP/service/auto_complete.py 与 `core/database.py::lifespan`：
//! - 启动后立即跑一轮
//! - 间隔 `AUTO_COMPLETE_INTERVAL_HOURS`（默认 24h）
//! - 扫描 DELIVERED 且最近一次发货超过 `AUTO_COMPLETE_THRESHOLD_DAYS`（默认 7 天）、
//!   且中间无返修的批次，逐个调用 `PartService::complete`
//! - 收到 `CancellationToken` 时优雅退出

use std::sync::Arc;
use std::time::Duration;

use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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

/// 一轮扫描 + 处理
async fn run_once(state: &Arc<AppState>, threshold_days: u32) -> anyhow::Result<()> {
    // TODO 业务实现阶段：
    // 1. SELECT 满足条件的批次
    // 2. 对每个批次开 transaction，调用 PartService::complete
    // 3. commit 后通过 state.ws_hub.broadcast(DashboardEvent) 推送给大屏
    let _ = (state, threshold_days);
    Ok(())
}