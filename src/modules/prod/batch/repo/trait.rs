//! 胖 trait `PartBatchRepoTrait`（17 方法）+ `impl for &mut PgConnection`。
//!
//! 2026-10-02 随 `t_part_batch` 归属迁入 prod 域，从单文件 `prod/batch/repo/queries.rs`
//! 拆出（单文件已超 conventions.md §2 的 1000 行上限）。签名零变化。
//!
//! 2026-10-03 集合读收口：删掉 2 条 `list.rs` 的镜像声明与转发 —— 原
//! `list_batches_with_part` / `count_batches_with_part`（待品检端点迁移后零调用方）。
//! 集合读一律**不进**胖 trait：service 走 ZST 的固有方法（`PartBatchRepo::yyy`），
//! 从不经 trait 侧。trait 只镜像**确有 trait 侧调用方**的方法。
//!
//! 2026-10-07 待品检队列读迁往 `prod::inspection`（`list_inspection_queue` /
//! `count_inspection_queue` 随之迁走），本 trait 声明数不变（那 2 条本来就**不在**
//! trait 里），零改动。
//!
//! ## 为什么是胖 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例；service 同时需要
//! `list_active_by_part_id_with_holder` 与 `list_batches_with_part_in_customers` 时
//! 无法表达「同连接两次借用」。胖 trait 是单借位。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 生成 `MockPartBatchRepoTrait` 供
//! service 单测注入。

use async_trait::async_trait;
use chrono::NaiveDateTime;
use sqlx::PgConnection;

use super::queries::{NewInitialBatch, PartBatchRepo};
use crate::modules::prod::batch::model::{PartBatchScanRow, RecentBatchRow, TPartBatch};
use crate::shared::error::AppError;

/// 把 `PartBatchRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `PartBatchRepo::yyy`，零转发壳
/// （与 iam 2026-09-22 删 `PgIamRepo` 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时
/// `self: &mut &mut PgConnection`，两次 deref 才得到 `PgConnection`：
/// `*self: &mut PgConnection`，`**self: PgConnection`，故喂给
/// `PartBatchRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PartBatchRepoTrait: Send {
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartBatch>, sqlx::Error>;
    async fn list_by_delivery_note(&mut self, note_id: i64)
    -> Result<Vec<TPartBatch>, sqlx::Error>;
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
    /// 返回真实影响行数：0 行 → 由 service 转 `VERSION_CONFLICT` 409。
    /// 详见 impl 处的「返回值契约」（2026-10-01 review 第 1 轮 M6）。
    #[allow(clippy::too_many_arguments)]
    async fn update<'a>(
        &mut self,
        batch_id: i64,
        version: i32,
        delivery_note_id: Option<i64>,
        status: Option<&'a str>,
        updated_by: Option<i64>,
    ) -> Result<u64, AppError>;
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
    async fn count_held_by_worker(&mut self, worker_id: i64) -> Result<i64, sqlx::Error>;
    async fn list_held_by_worker(&mut self, worker_id: i64)
    -> Result<Vec<TPartBatch>, sqlx::Error>;
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
}

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
    async fn update<'a>(
        &mut self,
        batch_id: i64,
        version: i32,
        delivery_note_id: Option<i64>,
        status: Option<&'a str>,
        updated_by: Option<i64>,
    ) -> Result<u64, AppError> {
        PartBatchRepo::update(
            &mut **self,
            batch_id,
            version,
            delivery_note_id,
            status,
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
        PartBatchRepo::list_batches_with_part_in_customers(
            &mut **self,
            statuses,
            customer_ids,
            limit,
        )
        .await
    }

    async fn list_recent_by_note(
        &mut self,
        note_id: i64,
        limit: i64,
    ) -> Result<Vec<RecentBatchRow>, sqlx::Error> {
        PartBatchRepo::list_recent_by_note(&mut **self, note_id, limit).await
    }

    async fn count_held_by_worker(&mut self, worker_id: i64) -> Result<i64, sqlx::Error> {
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
}
