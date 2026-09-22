//! statistics 域 repo 层 — SQL 真源 + 胖 trait + PG 实现（2026-09-23 PR8）
//!
//! ## 结构（2026-09-23 PR8：statistics 域新增 `repo/`）
//! - `sql.rs`：原 `repo.rs` 内 16 个固有静态方法全部迁出为 free fn（参数由
//!   `&mut PgConnection` 改为 `impl PgExecutor` 仅 `worker_pickup_rows` 一例；
//!   其余仍走 `&mut PgConnection`）。**SQL 字符串 byte-identical 与原 `repo.rs`
//!   1:1 平移**（PR8 硬约束 #8：零 SQL 文本变化）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `StatisticsRepoTrait`（16 方法合并单 trait），
//!   re-export `sql.rs` 的 4 个原始行结构；并直接 `impl StatisticsRepoTrait for
//!   &mut PgConnection`——handler/service 借 `&mut *tx` / `&mut *conn` 即可，
//!   零中间壳（与 iam 2026-09-22 删 `PgIamRepo` 同步；与 dashboard / shelf 同形）。
//!
//! ## 为什么是胖 trait 而不是按实体拆 4 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例。胖 trait `StatisticsRepoTrait`
//! 是单借位，service 签名 `<R: StatisticsRepoTrait>(&self, mut repo: R, ...)` 一次收下
//! （by-value；生产 `R = &mut PgConnection`，单测 `R = MockStatisticsRepoTrait`），
//! 方法体内全部 `repo.xxx()` 都走同一连接。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都
//! `DerefMut<Target = PgConnection>`，故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，
//! 可直接喂给 `sql::xxx`。无任何 `PgStatisticsRepo<'a>` 转发壳。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockStatisticsRepoTrait`
//! 供 service 单测注入。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs free fn 签名 1:1）。

pub mod sql;

// 重导出 sql.rs 中的 4 个原始行结构（service 装配层用到）。
pub use sql::{PickupSkipDetailRow, PickupSkipSummaryRow, WorkerPartRow, WorkerPickupRow};

use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgConnection;

/// statistics 域数据访问 trait（16 个聚合方法，对应 dashboard 5 端点 + 跳序取件 2 端点）。
///
/// 单 trait 而非每实体一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有多个 repo（与 iam / dashboard / shelf 同形）。
///
/// 方法签名 = `sql.rs` free fn，参数全收 `&mut self`（impl 在 `&mut PgConnection`
/// 上时 `self: &mut &mut PgConnection`，两次 deref：`&mut **self`）。
///
/// ## 跨域 ZST 借位：`repo.conn_mut()`
/// `worker_stats` / `worker_detail` 两个端点需要跨域 ZST（`WorkerRepo::xxx` /
/// `WorkTypeRepo::xxx`）。这些方法签名是 `&mut PgConnection`，不在 trait 内。
/// service 内通过 `repo.conn_mut()` 借位即可——本 trait 暴露 `conn_mut()` 访问器
/// （与 `DeliveryNoteRepoTrait::conn_mut` 2026-09-22 D-5 引入同形；iam / dashboard
/// 无跨域调用故无此 accessor）。生产实现（`&mut PgConnection`）直返 `self`；
/// mock 测试不调用此方法。
///
/// ## 16 个方法按业务分组
/// - tab1 基础计数（9）：count_created / count_completed / count_in_process_at /
///   delivered_stats / daily_created_counts / daily_completed_counts /
///   count_repair_parts / count_overdue_undelivered / status_distribution
/// - tab2 工人 pickup 聚合（1）：worker_pickup_rows
/// - tab3 单工人详情（3）：worker_detail_events / worker_daily_pickups / worker_parts
/// - tab4 跳序取件（3）：pickup_skip_summary / pickup_skip_detail / pickup_skip_detail_count
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait StatisticsRepoTrait: Send {
    /// 跨域 ZST 借位访问器：返回 `&mut PgConnection`，
    /// 供 `WorkerRepo::xxx(&mut *repo.conn_mut(), ...)` / `WorkTypeRepo::xxx(...)` 等。
    /// 生产实现（`&mut PgConnection`）直返 `self`；mock 测试不调用此方法。
    fn conn_mut(&mut self) -> &mut PgConnection;

    // ── tab1 基础计数（9）──
    async fn count_created(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<i64, sqlx::Error>;
    async fn count_completed(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<i64, sqlx::Error>;
    async fn count_in_process_at(&mut self, date_to: NaiveDate) -> Result<i64, sqlx::Error>;
    async fn delivered_stats(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<(i64, Decimal, i64, i64), sqlx::Error>;
    async fn daily_created_counts(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error>;
    async fn daily_completed_counts(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error>;
    async fn count_repair_parts(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<i64, sqlx::Error>;
    async fn count_overdue_undelivered(&mut self, today: NaiveDate) -> Result<i64, sqlx::Error>;
    async fn status_distribution(&mut self) -> Result<Vec<(String, i64)>, sqlx::Error>;

    // ── tab2 工人 pickup 聚合（1）──
    async fn worker_pickup_rows(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<WorkerPickupRow>, sqlx::Error>;

    // ── tab3 单工人详情（3）──
    async fn worker_detail_events(
        &mut self,
        worker_id: i64,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<(i64, i64, i64), sqlx::Error>;
    async fn worker_daily_pickups(
        &mut self,
        worker_id: i64,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error>;
    async fn worker_parts(
        &mut self,
        worker_id: i64,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<WorkerPartRow>, sqlx::Error>;

    // ── tab4 跳序取件（3）──
    async fn pickup_skip_summary(&mut self) -> Result<Vec<PickupSkipSummaryRow>, sqlx::Error>;
    async fn pickup_skip_detail(
        &mut self,
        worker_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PickupSkipDetailRow>, sqlx::Error>;
    async fn pickup_skip_detail_count(&mut self, worker_id: i64) -> Result<i64, sqlx::Error>;
}

/// 把 `StatisticsRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::xxx`，零转发壳（与 iam / dashboard /
/// shelf 2026-09-22 重构同形）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::xxx` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl StatisticsRepoTrait for &mut PgConnection {
    fn conn_mut(&mut self) -> &mut PgConnection {
        self
    }

    // ── tab1 基础计数（9）──
    async fn count_created(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<i64, sqlx::Error> {
        sql::count_created(&mut **self, date_from, date_to).await
    }
    async fn count_completed(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<i64, sqlx::Error> {
        sql::count_completed(&mut **self, date_from, date_to).await
    }
    async fn count_in_process_at(&mut self, date_to: NaiveDate) -> Result<i64, sqlx::Error> {
        sql::count_in_process_at(&mut **self, date_to).await
    }
    async fn delivered_stats(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<(i64, Decimal, i64, i64), sqlx::Error> {
        sql::delivered_stats(&mut **self, date_from, date_to).await
    }
    async fn daily_created_counts(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error> {
        sql::daily_created_counts(&mut **self, date_from, date_to).await
    }
    async fn daily_completed_counts(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error> {
        sql::daily_completed_counts(&mut **self, date_from, date_to).await
    }
    async fn count_repair_parts(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<i64, sqlx::Error> {
        sql::count_repair_parts(&mut **self, date_from, date_to).await
    }
    async fn count_overdue_undelivered(&mut self, today: NaiveDate) -> Result<i64, sqlx::Error> {
        sql::count_overdue_undelivered(&mut **self, today).await
    }
    async fn status_distribution(&mut self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        sql::status_distribution(&mut **self).await
    }

    // ── tab2 工人 pickup 聚合（1）──
    async fn worker_pickup_rows(
        &mut self,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<WorkerPickupRow>, sqlx::Error> {
        sql::worker_pickup_rows(&mut **self, date_from, date_to).await
    }

    // ── tab3 单工人详情（3）──
    async fn worker_detail_events(
        &mut self,
        worker_id: i64,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<(i64, i64, i64), sqlx::Error> {
        sql::worker_detail_events(&mut **self, worker_id, date_from, date_to).await
    }
    async fn worker_daily_pickups(
        &mut self,
        worker_id: i64,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error> {
        sql::worker_daily_pickups(&mut **self, worker_id, date_from, date_to).await
    }
    async fn worker_parts(
        &mut self,
        worker_id: i64,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<Vec<WorkerPartRow>, sqlx::Error> {
        sql::worker_parts(&mut **self, worker_id, date_from, date_to).await
    }

    // ── tab4 跳序取件（3）──
    async fn pickup_skip_summary(&mut self) -> Result<Vec<PickupSkipSummaryRow>, sqlx::Error> {
        sql::pickup_skip_summary(&mut **self).await
    }
    async fn pickup_skip_detail(
        &mut self,
        worker_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PickupSkipDetailRow>, sqlx::Error> {
        sql::pickup_skip_detail(&mut **self, worker_id, limit, offset).await
    }
    async fn pickup_skip_detail_count(&mut self, worker_id: i64) -> Result<i64, sqlx::Error> {
        sql::pickup_skip_detail_count(&mut **self, worker_id).await
    }
}
