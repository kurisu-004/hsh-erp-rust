//! dashboard 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 Group E 重构：dashboard 域新增 `repo/`）
//! - `sql.rs`：原 `service.rs` 内 inline SQL 全文搬迁到 ZST struct `DashboardRepo`
//!   的 4 个固有静态方法（`snapshot_counters` / `snapshot_top_parts` /
//!   `snapshot_recent_batches` / `snapshot_workers`），**SQL 字符串零 diff**
//!   （`grep` 原 service.rs 对比一致）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `DashboardRepoTrait`（4 方法合并单 trait），
//!   re-export `sql.rs` 的 model / struct / row；并直接
//!   `impl DashboardRepoTrait for &mut PgConnection`——handler/service 借
//!   `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么是胖 trait 而不是按实体拆 4 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例。胖 trait `DashboardRepoTrait`
//! 是单借位，service 签名 `<R: DashboardRepoTrait>(&self, mut repo: R, ...)` 一次收下
//! （by-value；生产路径 `R = &mut PgConnection`，单测 `R = MockDashboardRepoTrait`），
//! 方法体内全部 `repo.xxx()` 都走同一连接。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都
//! `DerefMut<Target = PgConnection>`，故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，
//! 可直接喂给 `sql::DashboardRepo::yyy`。无任何 `PgDashboardRepo<'a>` 转发壳
//! （与 iam 2026-09-22 删 `PgIamRepo` 同步；与 shelf 2026-09-22 同形）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockDashboardRepoTrait`
//! 供 service 单测注入。方法签名里的 `<'a>` 显式生命周期是 mockall 0.15 + async_trait
//! 的硬性要求。dashboard 域当前无 mod tests（service 全部走
//! `tests/dashboard_ws_api.rs` 集成测试守护），故未建 `service_tests/` 子目录——按
//! conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1）。

use async_trait::async_trait;
use sqlx::PgConnection;
use std::collections::HashMap;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct / 中间聚合结构 / 行精简类型。
// `super::repo::{DashboardRepo, TopPartsData, RecentBatchesData, BatchLite, PartLite}`
// 路径不破（service 装配层会用到）。
pub use sql::{
    BatchLite, DashboardRepo, PartLite, RecentBatchesData, TopPartsData,
};

/// dashboard 域数据访问 trait（4 个聚合方法，对应 dashboard snapshot 的 4 个维度）。
///
/// 单 trait 而非每实体一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有多个 repo（2026-09-22 Group E 重构定案；与 iam / shelf 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法，参数全收 `&mut self`（impl 在 `&mut PgConnection`
/// 上时 `self: &mut &mut PgConnection`，两次 deref：`&mut **self`）。`<'a>` 显式生命周期
/// 是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
///
/// ## dashboard 自封闭
/// 4 个聚合方法全 dashboard 内，不跨域 SQL（part / customer / worker / process 等表都是
/// 跨表 join，不算跨模块静态调用）。handler / service 直接借 `&mut *tx` 喂给 trait 即可。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait DashboardRepoTrait: Send {
    /// 未来 N 天交付分桶（counter buckets）。0 计数天也填充（保证 7 天固定 7 条）。
    async fn snapshot_counters(&mut self, days: i64)
        -> Result<Vec<crate::modules::dashboard::dto::UpcomingDeliveryBucket>, sqlx::Error>;

    /// 产线架 + IN_PROCESS 批次 + 品检区批次 + 客户 / 工序 名字查表。
    /// 一次调用拉全量，返回 `TopPartsData` 富结构；service 内部聚合 → `OnProductionShelfGroup`。
    async fn snapshot_top_parts(&mut self, top_n: i64)
        -> Result<TopPartsData, sqlx::Error>;

    /// 工人持有 IN_PROCESS + 客户路径 + PICKED_UP 时间。
    /// 返回 `RecentBatchesData` 富结构；service 内部聚合 → `in_process` items。
    async fn snapshot_recent_batches(&mut self, top_n: i64)
        -> Result<RecentBatchesData, sqlx::Error>;

    /// 工人 id → name 映射（worker-held items 用），service 二次装配时按 holder_id 查名字。
    async fn snapshot_workers(&mut self, ids: &[i64])
        -> Result<HashMap<i64, String>, sqlx::Error>;
}

/// 把 `DashboardRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::DashboardRepo::yyy`，零转发壳
///（与 iam 2026-09-22 删 `PgIamRepo` 同步；与 shelf 2026-09-22 同形）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::DashboardRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl DashboardRepoTrait for &mut PgConnection {
    async fn snapshot_counters(
        &mut self,
        days: i64,
    ) -> Result<Vec<crate::modules::dashboard::dto::UpcomingDeliveryBucket>, sqlx::Error> {
        DashboardRepo::snapshot_counters(&mut **self, days).await
    }

    async fn snapshot_top_parts(
        &mut self,
        top_n: i64,
    ) -> Result<TopPartsData, sqlx::Error> {
        DashboardRepo::snapshot_top_parts(&mut **self, top_n).await
    }

    async fn snapshot_recent_batches(
        &mut self,
        top_n: i64,
    ) -> Result<RecentBatchesData, sqlx::Error> {
        DashboardRepo::snapshot_recent_batches(&mut **self, top_n).await
    }

    async fn snapshot_workers(
        &mut self,
        ids: &[i64],
    ) -> Result<HashMap<i64, String>, sqlx::Error> {
        DashboardRepo::snapshot_workers(&mut **self, ids).await
    }
}