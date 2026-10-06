//! dashboard 域交期 service（2026-10-07 从 `service/snapshot.rs` 拆出）
//!
//! 两个方法分别服务两个独立端点：
//! - `build_upcoming_buckets` —— `GET /api/v2/dashboard/upcoming-delivery`（柱状图 +
//!   「今日到期」「N 天到期」两个 KPI 的唯一数据源）
//! - `build_delivery_order_details` —— `GET /api/v2/dashboard/delivery-orders`
//!   （柱状图某一天的下钻抽屉）
//!
//! ## 参数收敛策略
//! `days` 与 `basis` 的缺省值 / clamp 全部收在 service 层（唯一决策点），repo 层收
//! 确定值、不做兜底。

use chrono::NaiveDate;

use crate::infra::clock::now_naive;
use crate::modules::dashboard::dto::DeliveryBasis;
use crate::modules::dashboard::repo::{DELIVERY_DETAIL_LIMIT, DashboardRepoTrait};
use crate::modules::dashboard::vo::{DeliveryOrderDetailOut, UpcomingDeliveryBuckets};

/// 未来交付分桶默认天数（对齐前端 dashboard 视图横轴默认宽度）
pub const DASHBOARD_DEFAULT_DAYS: i64 = 14;

/// 未来交付分桶最大天数（防御恶意大数 / 拼写错把日期塞成 10000）
pub const DASHBOARD_MAX_DAYS: i64 = 60;

/// 未来交付分桶最小天数（防御 0 / 负数 / 拼写错）
pub const DASHBOARD_MIN_DAYS: i64 = 1;

impl super::DashboardService {
    /// 未来 N 天交期分桶。`today` 由本方法取一次时钟，同时喂给 repo 与响应 VO，
    /// 保证 `buckets[0].date` 与 `today` 恒等。
    pub async fn build_upcoming_buckets<R: DashboardRepoTrait>(
        &self,
        mut repo: R,
        days: Option<i64>,
        basis: Option<DeliveryBasis>,
    ) -> Result<UpcomingDeliveryBuckets, sqlx::Error> {
        let days = days
            .unwrap_or(DASHBOARD_DEFAULT_DAYS)
            .clamp(DASHBOARD_MIN_DAYS, DASHBOARD_MAX_DAYS);
        // 2026-10-07：口径缺省从 `Planned` 改为 `System`（前端默认显示系统交期，
        // 后端缺省与之保持一致）。
        let basis = basis.unwrap_or_default();
        let today = now_naive().date();

        let buckets = repo.snapshot_counters(today, days, basis).await?;

        Ok(UpcomingDeliveryBuckets {
            today: today.format("%Y-%m-%d").to_string(),
            buckets,
            ts: crate::infra::clock::now_shanghai_iso(),
        })
    }

    /// 柱状图下钻抽屉：单日 + 状态过滤的工单明细。
    pub async fn build_delivery_order_details<R: DashboardRepoTrait>(
        &self,
        mut repo: R,
        date: NaiveDate,
        statuses: Vec<String>,
        basis: Option<DeliveryBasis>,
    ) -> Result<DeliveryOrderDetailOut, sqlx::Error> {
        let basis = basis.unwrap_or_default();
        let status_refs: Vec<&str> = statuses.iter().map(String::as_str).collect();
        repo.list_delivery_order_details(date, &status_refs, basis, DELIVERY_DETAIL_LIMIT)
            .await
    }
}
