use async_trait::async_trait;
use sqlx::PgConnection;
use std::collections::HashMap;

use crate::modules::dashboard::dto::DeliveryBasis;

pub mod sql;

pub use sql::{BatchLite, DashboardRepo, PartLite, RecentBatchesData, TopPartsData};


#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait DashboardRepoTrait: Send {
    async fn snapshot_counters(
        &mut self,
        days: i64,
        basis: DeliveryBasis,
    ) -> Result<Vec<crate::modules::dashboard::vo::UpcomingDeliveryBucket>, sqlx::Error>;

    /// 产线架 + IN_PROCESS 批次 + 品检区批次 + 客户 / 工序 名字查表。
    /// 一次调用拉全量，返回 `TopPartsData` 富结构；service 内部聚合 → `OnProductionShelfGroup`。
    async fn snapshot_top_parts(&mut self, top_n: i64) -> Result<TopPartsData, sqlx::Error>;

    /// 工人持有 IN_PROCESS + 客户路径 + PICKED_UP 时间。
    /// 返回 `RecentBatchesData` 富结构；service 内部聚合 → `in_process` items。
    async fn snapshot_recent_batches(
        &mut self,
        top_n: i64,
    ) -> Result<RecentBatchesData, sqlx::Error>;

    /// 工人 id → name 映射（worker-held items 用），service 二次装配时按 holder_id 查名字。
    async fn snapshot_workers(&mut self, ids: &[i64]) -> Result<HashMap<i64, String>, sqlx::Error>;
}


#[async_trait]
impl DashboardRepoTrait for &mut PgConnection {
    async fn snapshot_counters(
        &mut self,
        days: i64,
        basis: DeliveryBasis,
    ) -> Result<Vec<crate::modules::dashboard::vo::UpcomingDeliveryBucket>, sqlx::Error> {
        DashboardRepo::snapshot_counters(&mut **self, days, basis).await
    }

    async fn snapshot_top_parts(&mut self, top_n: i64) -> Result<TopPartsData, sqlx::Error> {
        DashboardRepo::snapshot_top_parts(&mut **self, top_n).await
    }

    async fn snapshot_recent_batches(
        &mut self,
        top_n: i64,
    ) -> Result<RecentBatchesData, sqlx::Error> {
        DashboardRepo::snapshot_recent_batches(&mut **self, top_n).await
    }

    async fn snapshot_workers(&mut self, ids: &[i64]) -> Result<HashMap<i64, String>, sqlx::Error> {
        DashboardRepo::snapshot_workers(&mut **self, ids).await
    }
}
