//! dashboard 域交期分桶 VO（2026-10-07 从 DashboardSnapshot 拆到独立端点）

use serde::Serialize;

/// 交期分桶响应：柱状图 + 「今日到期」「N 天到期」两个 KPI 的唯一数据源
#[derive(Debug, Clone, Serialize)]
pub struct UpcomingDeliveryBuckets {
    /// 后端判定的「今天」（YYYY-MM-DD）。2026-10-07 起因前端 new Date() 与后端
    /// 时区可能不同步导致柱状图整体错位、桶一个都匹配不上而恒空，改由后端下发。
    /// 口径 = `infra::clock::now_naive()`（Asia/Shanghai），与 DB 写入同源；
    /// service 取一次时钟后同时喂给 repo 的 SQL 窗口下界与本字段（不各自取一次
    /// 时钟），故桶序列的起点恒等于本字段。
    pub today: String,
    /// 恒为请求的 days 条，缺失日期已在 Rust 侧零填充
    pub buckets: Vec<super::snapshot::UpcomingDeliveryBucket>,
    pub ts: String,
}
