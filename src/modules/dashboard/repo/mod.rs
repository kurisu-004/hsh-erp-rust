use async_trait::async_trait;
use chrono::NaiveDate;
use sqlx::PgConnection;
use std::collections::HashMap;

use crate::modules::dashboard::dto::DeliveryBasis;
use crate::modules::dashboard::vo::{
    DeliveryOrderDetailOut, SystemDeliveryOrders, UpcomingDeliveryBucket,
};

pub mod delivery;
pub mod sql;

pub use delivery::{DELIVERY_BUCKET_LIMIT, DELIVERY_DETAIL_LIMIT, DELIVERY_STATUSES, DeliveryRepo};
pub use sql::{BatchLite, DashboardRepo, PartLite, RecentBatchesData};

#[async_trait]
pub trait DashboardRepoTrait: Send {
    /// 未来 N 天交期分桶（柱状图 + 「今日到期」「N 天到期」两个 KPI）。
    ///
    /// `today` 由 service 取一次时钟传进来：桶序列的起点必须与 VO 的 `today` 字段
    /// 是同一个值，各自取一次会在跨零点窗口内给出两个不同的「今天」。
    async fn snapshot_counters(
        &mut self,
        today: NaiveDate,
        days: i64,
        basis: DeliveryBasis,
    ) -> Result<Vec<UpcomingDeliveryBucket>, sqlx::Error>;

    /// 品检区待品检批次数（前端「在检」KPI 只要一个数字）。
    async fn snapshot_in_inspection_count(&mut self) -> Result<i64, sqlx::Error>;

    /// 工人在手的 IN_PROCESS 批次（`location = 'WORKER'`）。
    /// 返回 `RecentBatchesData`；service 内部装配成 `in_process`。
    async fn snapshot_recent_batches(&mut self) -> Result<RecentBatchesData, sqlx::Error>;

    /// 工人 id → name 映射（`in_process` 用），service 二次装配时按 holder_id 查名字。
    async fn snapshot_workers(&mut self, ids: &[i64]) -> Result<HashMap<i64, String>, sqlx::Error>;

    /// 逾期未交工单数（工单级：装配件算 1 条，子件不重复计入）。
    async fn snapshot_overdue(&mut self, today: NaiveDate) -> Result<i64, sqlx::Error>;

    /// 最紧急工单 + 部分已交两桶（`delivered_quantity` 判据在 repo 侧）。
    async fn snapshot_system_delivery_orders(
        &mut self,
        today: NaiveDate,
    ) -> Result<SystemDeliveryOrders, sqlx::Error>;

    /// 柱状图下钻抽屉：单日 + 状态过滤的工单明细。
    async fn list_delivery_order_details(
        &mut self,
        date: NaiveDate,
        statuses: &[&str],
        basis: DeliveryBasis,
        limit: i64,
    ) -> Result<DeliveryOrderDetailOut, sqlx::Error>;
}

#[async_trait]
impl DashboardRepoTrait for &mut PgConnection {
    async fn snapshot_counters(
        &mut self,
        today: NaiveDate,
        days: i64,
        basis: DeliveryBasis,
    ) -> Result<Vec<UpcomingDeliveryBucket>, sqlx::Error> {
        DashboardRepo::snapshot_counters(&mut **self, today, days, basis).await
    }

    async fn snapshot_in_inspection_count(&mut self) -> Result<i64, sqlx::Error> {
        DashboardRepo::count_inspection_batches(&mut **self).await
    }

    async fn snapshot_recent_batches(&mut self) -> Result<RecentBatchesData, sqlx::Error> {
        DashboardRepo::snapshot_recent_batches(&mut **self).await
    }

    async fn snapshot_workers(&mut self, ids: &[i64]) -> Result<HashMap<i64, String>, sqlx::Error> {
        DashboardRepo::snapshot_workers(&mut **self, ids).await
    }

    async fn snapshot_overdue(&mut self, today: NaiveDate) -> Result<i64, sqlx::Error> {
        DeliveryRepo::count_overdue(&mut **self, today).await
    }

    async fn snapshot_system_delivery_orders(
        &mut self,
        today: NaiveDate,
    ) -> Result<SystemDeliveryOrders, sqlx::Error> {
        DeliveryRepo::list_system_delivery_orders(&mut **self, today, DELIVERY_BUCKET_LIMIT).await
    }

    async fn list_delivery_order_details(
        &mut self,
        date: NaiveDate,
        statuses: &[&str],
        basis: DeliveryBasis,
        limit: i64,
    ) -> Result<DeliveryOrderDetailOut, sqlx::Error> {
        DeliveryRepo::list_delivery_order_details(&mut **self, date, statuses, basis, limit).await
    }
}
