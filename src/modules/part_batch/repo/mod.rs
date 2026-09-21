//! part_batch 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-1 重构对齐 iam / shelf / customer 范本）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，13 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `list.rs`：原 `repo_list.rs` 全文搬迁，2 个 list/count 方法（`list_batches_with_part` /
//!   `count_batches_with_part`），与 `sql.rs` 同 ZST `PartBatchRepo` 的 impl 块。
//!   单独文件是为避免 `sql.rs` 触线 conventions.md §2 硬上限 1000 行。
//! - `mod.rs`（本文件）：对外暴露胖 trait `PartBatchRepoTrait`（15 方法合并单 trait；
//!   t_part_batch 13 + 列表查询 2），并直接 `impl PartBatchRepoTrait for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么是胖 trait（15 方法）而非按子模块拆 2 trait
//! 与 iam / shelf / customer 范本同形：`&mut PgConnection` 同一作用域只能借给一
//! 个 repo 实例；service 同时使用 `list_active_by_part_id_with_holder`（来自 sql.rs）
//! 与 `list_batches_with_part`（来自 list.rs）时无法表达「同连接两次借用」。
//! 胖 trait 是单借位，service 签名 `<R: PartBatchRepoTrait>(&self, mut repo: R, ...)`
//! 一次收下（by-value；生产 `R = &mut PgConnection`，单测 `R = MockPartBatchRepo`）。
//!
//! ## 为什么 trait 命名为 `PartBatchRepoTrait`（带 `Trait` 后缀）
//! 跨模块静态调用方 11 处直接走 ZST 静态方法（part 域 8 + worker_pool 域 3 + 注释
//! 引用 1），本任务**不能**破坏 `part_batch::repo::PartBatchRepo` 作为 ZST 的对外
//! 身份，故 trait 改名 `PartBatchRepoTrait`（与 shelf / customer 范本同形）：
//!
//! - `part_batch::repo::PartBatchRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::PartBatchRepo;` 重新导出至本模块），保留 13 + 2 个 pub 静态
//!   方法签名不变（cross-module 调用方零修改）。
//! - `part_batch::repo::PartBatchRepoTrait` —— 本文件新加的胖 trait（15 方法合并单 trait），
//!   part_batch 域内部 service 用 `<R: PartBatchRepoTrait>` 收（注意：part_batch 当前
//!   无内联 service，被其他域 service 直调；本任务**不**改其他域调用方，沿用旧
//!   `PartBatchRepo::xxx(&mut *conn, ...)` ZST 静态调用形态即可编译通过；后续 D-2
//!   重构其他域时再统一改 trait 注入）。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::PartBatchRepo::yyy`。
//! 无需任何 `PgPartBatchRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockPartBatchRepo`
//! 供 service 单测注入。part_batch 域当前无内联 mod tests（service 全部走
//!   `tests/part_batch_api.rs` + `tests/delivery_attach_batches_api.rs` 等集成测试守护），
//!   故未建 `part_batch/service_tests/` 目录——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs / list.rs 签名 1:1；唯一特殊：
//! `split_batch` 内部守卫 0 行 → `sqlx::Error::RowNotFound`，由 caller map）。

use async_trait::async_trait;
use chrono::{NaiveDate, NaiveDateTime};
use sqlx::PgConnection;

pub mod list;
pub mod sql;

// 重导出 sql.rs 中的 ZST struct / insert / row 与 model 表行类型，让上层继续用
// `super::repo::{TPartBatch, PartBatchScanRow, RecentBatchRow, PartBatchRepo, NewInitialBatch}`
// 这种路径不破（cross-module 调用方都依赖这条路径）。
pub use super::model::{InspectionBatchListRow, PartBatchScanRow, RecentBatchRow, TPartBatch};
pub use sql::{NewInitialBatch, PartBatchRepo};

/// part_batch 域数据访问 trait（15 方法 = t_part_batch CRUD 13 + list/count 2）。
///
/// 单 trait 而非每子模块一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 D-1 重构定案；与 iam / shelf /
/// customer 同形）。
///
/// 方法签名 = `sql.rs` / `list.rs` 固有静态方法去 executor 形参。`<'a>` 显式
/// 生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
///
/// 注：`split_batch` 与 `_split_batch_inner` 不进 trait（收 `&mut PgConnection`
/// 多次借用同一连接，且仅作为 part 域 ZST 静态调用方的薄包装），调用方仍走
/// `PartBatchRepo::split_batch(&mut *conn, ...)`。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PartBatchRepoTrait: Send {
    // ── t_part_batch CRUD（11）──
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartBatch>, sqlx::Error>;
    async fn list_by_delivery_note(
        &mut self,
        note_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error>;
    async fn list_with_part_by_delivery_note(
        &mut self,
        note_id: i64,
    ) -> Result<Vec<(TPartBatch, crate::modules::part::model::TPart)>, sqlx::Error>;
    async fn list_with_part_by_delivery_note_ids<'a>(
        &mut self,
        note_ids: &'a [i64],
    ) -> Result<Vec<(TPartBatch, crate::modules::part::model::TPart)>, sqlx::Error>;
    async fn list_by_part_ids<'a>(
        &mut self,
        part_ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<TPartBatch>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update<'a>(
        &mut self,
        batch_id: i64,
        version: i32,
        delivery_note_id: Option<i64>,
        status: Option<&'a str>,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn attach_to_note(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        note_id: i64,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn list_active_by_part_id(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error>;
    async fn list_active_by_part_id_with_holder(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<PartBatchScanRow>, sqlx::Error>;
    async fn list_active_by_part_ids<'a>(
        &mut self,
        part_ids: &'a [i64],
    ) -> Result<Vec<TPartBatch>, sqlx::Error>;
    async fn list_batches_with_part_in_customers<'a, 'b>(
        &mut self,
        statuses: &'a [&'a str],
        customer_ids: &'b [i64],
        limit: i64,
    ) -> Result<Vec<(TPartBatch, crate::modules::part::model::TPart)>, sqlx::Error>;
    async fn list_recent_by_note(
        &mut self,
        note_id: i64,
        limit: i64,
    ) -> Result<Vec<RecentBatchRow>, sqlx::Error>;
    async fn count_held_by_worker(
        &mut self,
        worker_id: i64,
    ) -> Result<i64, sqlx::Error>;
    async fn list_held_by_worker(
        &mut self,
        worker_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error>;
    async fn find_delivered_older_than(
        &mut self,
        threshold: NaiveDateTime,
    ) -> Result<Vec<(i64, i64, i32)>, sqlx::Error>;
    async fn create_initial_batch<'a>(
        &mut self,
        new: NewInitialBatch<'a>,
    ) -> Result<i64, sqlx::Error>;
    async fn has_active_batch_on_delivery_note(
        &mut self,
        part_id: i64,
    ) -> Result<bool, sqlx::Error>;

    // ── 待检批次列表 / COUNT（2）── 来自 list.rs
    #[allow(clippy::too_many_arguments)]
    async fn list_batches_with_part<'a, 'b>(
        &mut self,
        statuses: &'a [&'a str],
        customer_ids: &'b [i64],
        keyword: Option<&'b str>,
        serial_no: Option<&'b str>,
        date_from: Option<NaiveDate>,
        date_to: Option<NaiveDate>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<InspectionBatchListRow>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn count_batches_with_part<'a, 'b>(
        &mut self,
        statuses: &'a [&'a str],
        customer_ids: &'b [i64],
        keyword: Option<&'b str>,
        serial_no: Option<&'b str>,
        date_from: Option<NaiveDate>,
        date_to: Option<NaiveDate>,
    ) -> Result<i64, sqlx::Error>;
}

/// 把 `PartBatchRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::PartBatchRepo::yyy`，零转发壳
/// （与 iam 2026-09-22 删 `PgIamRepo` 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时
/// `self: &mut &mut PgConnection`，两次 deref 才得到 `PgConnection`：
/// `*self: &mut PgConnection`，`**self: PgConnection`，故喂给
/// `sql::PartBatchRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl PartBatchRepoTrait for &mut PgConnection {
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        PartBatchRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn list_by_delivery_note(
        &mut self,
        note_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        PartBatchRepo::list_by_delivery_note(&mut **self, note_id).await
    }

    async fn list_with_part_by_delivery_note(
        &mut self,
        note_id: i64,
    ) -> Result<Vec<(TPartBatch, crate::modules::part::model::TPart)>, sqlx::Error> {
        PartBatchRepo::list_with_part_by_delivery_note(&mut **self, note_id).await
    }

    async fn list_with_part_by_delivery_note_ids<'b>(
        &mut self,
        note_ids: &'b [i64],
    ) -> Result<Vec<(TPartBatch, crate::modules::part::model::TPart)>, sqlx::Error> {
        PartBatchRepo::list_with_part_by_delivery_note_ids(&mut **self, note_ids).await
    }

    async fn list_by_part_ids<'b>(
        &mut self,
        part_ids: &'b [i64],
        include_deleted: bool,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        PartBatchRepo::list_by_part_ids(&mut **self, part_ids, include_deleted).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn update<'b>(
        &mut self,
        batch_id: i64,
        version: i32,
        delivery_note_id: Option<i64>,
        status: Option<&'b str>,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        PartBatchRepo::update(
            &mut **self,
            batch_id,
            version,
            delivery_note_id,
            status,
            when,
            updated_by,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn attach_to_note(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        note_id: i64,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        PartBatchRepo::attach_to_note(
            &mut **self,
            batch_id,
            expected_version,
            note_id,
            when,
            updated_by,
        )
        .await
    }

    async fn list_active_by_part_id(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        PartBatchRepo::list_active_by_part_id(&mut **self, part_id).await
    }

    async fn list_active_by_part_id_with_holder(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<PartBatchScanRow>, sqlx::Error> {
        PartBatchRepo::list_active_by_part_id_with_holder(&mut **self, part_id).await
    }

    async fn list_active_by_part_ids<'b>(
        &mut self,
        part_ids: &'b [i64],
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        PartBatchRepo::list_active_by_part_ids(&mut **self, part_ids).await
    }

    async fn list_batches_with_part_in_customers<'b, 'c>(
        &mut self,
        statuses: &'b [&'b str],
        customer_ids: &'c [i64],
        limit: i64,
    ) -> Result<Vec<(TPartBatch, crate::modules::part::model::TPart)>, sqlx::Error> {
        PartBatchRepo::list_batches_with_part_in_customers(&mut **self, statuses, customer_ids, limit)
            .await
    }

    async fn list_recent_by_note(
        &mut self,
        note_id: i64,
        limit: i64,
    ) -> Result<Vec<RecentBatchRow>, sqlx::Error> {
        PartBatchRepo::list_recent_by_note(&mut **self, note_id, limit).await
    }

    async fn count_held_by_worker(
        &mut self,
        worker_id: i64,
    ) -> Result<i64, sqlx::Error> {
        PartBatchRepo::count_held_by_worker(&mut **self, worker_id).await
    }

    async fn list_held_by_worker(
        &mut self,
        worker_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        PartBatchRepo::list_held_by_worker(&mut **self, worker_id).await
    }

    async fn find_delivered_older_than(
        &mut self,
        threshold: NaiveDateTime,
    ) -> Result<Vec<(i64, i64, i32)>, sqlx::Error> {
        PartBatchRepo::find_delivered_older_than(&mut **self, threshold).await
    }

    async fn create_initial_batch<'b>(
        &mut self,
        new: NewInitialBatch<'b>,
    ) -> Result<i64, sqlx::Error> {
        PartBatchRepo::create_initial_batch(&mut **self, new).await
    }

    async fn has_active_batch_on_delivery_note(
        &mut self,
        part_id: i64,
    ) -> Result<bool, sqlx::Error> {
        PartBatchRepo::has_active_batch_on_delivery_note(&mut **self, part_id).await
    }

    // ── list.rs（2）──
    #[allow(clippy::too_many_arguments)]
    async fn list_batches_with_part<'b, 'c>(
        &mut self,
        statuses: &'b [&'b str],
        customer_ids: &'c [i64],
        keyword: Option<&'c str>,
        serial_no: Option<&'c str>,
        date_from: Option<NaiveDate>,
        date_to: Option<NaiveDate>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<InspectionBatchListRow>, sqlx::Error> {
        PartBatchRepo::list_batches_with_part(
            &mut **self,
            statuses,
            customer_ids,
            keyword,
            serial_no,
            date_from,
            date_to,
            limit,
            offset,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn count_batches_with_part<'b, 'c>(
        &mut self,
        statuses: &'b [&'b str],
        customer_ids: &'c [i64],
        keyword: Option<&'c str>,
        serial_no: Option<&'c str>,
        date_from: Option<NaiveDate>,
        date_to: Option<NaiveDate>,
    ) -> Result<i64, sqlx::Error> {
        PartBatchRepo::count_batches_with_part(
            &mut **self,
            statuses,
            customer_ids,
            keyword,
            serial_no,
            date_from,
            date_to,
        )
        .await
    }
}