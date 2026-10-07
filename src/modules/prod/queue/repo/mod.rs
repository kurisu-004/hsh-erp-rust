//! queue 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-2 重构对齐 iam / shelf / customer / worker 范本）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，4 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `QueueRepoTrait`（本域 3 方法 + 跨域
//!   helper），并直接 `impl QueueRepoTrait for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//! - `dispatch.rs` —— ZST `QueueDispatchRepo`：「PENDING 批次下发给车间」专用查询
//!   （2026-10-08 自 `prod::batch::repo` 搬入，见该文件 doc）
//!
//! ## 为什么 trait 命名为 `QueueRepoTrait`（带 `Trait` 后缀）
//! 本任务范围内 `_e2e` / `statistics` / 其他域对 queue repo 的跨模块静态调用
//! 命中数为 0（grep 校验：除 `src/modules/prod/queue/` 自身外无
//! `QueueRepo::` 调用），但仍按 shelf / customer / process_chain / worker 范本
//! 命名 `*Trait` 后缀——未来跨模块调用方零修改成本（trait 改名比 ZST 改名风险低）。
//!
//! - `prod::queue::repo::QueueRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::QueueRepo;` 重新导出至本模块），保留 4 个 pub 静态方法签名
//!   不变（任何未来 cross-module 调用方零修改）。
//! - `prod::queue::repo::QueueRepoTrait` —— 本文件新加的胖 trait
//!   （18 方法 = 本域 4 + 跨域 helper 14），queue 域内部 service 用
//!   `<R: QueueRepoTrait>` 收。跨域 helper 的实时清单见下方「跨域 helper 清单」。
//!
//! ## 为什么是胖 trait（含本域 + 跨域 helper）
//! queue 是跨域核心（CLAUDE.md §「part 是跨域枢纽」的兄弟节点）——同 service
//! 方法（`refill_for_worker_with_work_type` / `compute_state` / `admin_remove_held_batch` /
//! `auto_allocate_for_process` / `assign_batch_to_worker`）需交替访问
//! t_queue（本域）+ t_worker + t_work_type + t_process + t_process_chain_step +
//! t_part_batch + t_part 共 7 表。
//!
//! 与 assembly / shelf / process_chain 同形：跨域 SQL 全部下沉到 `QueueRepoTrait`
//! helper 方法，trait impl 一行委托到对应 ZST 静态方法（已重构域）或原 ZST 静态方法
//! （part 域，D-6 未做）。
//!
//! ## 跨域 helper 清单（11）
//! - worker (3)：`worker_get_by_id` / `worker_list_active_by_process_id` /
//!   `worker_list_with_filters_for_seed`
//!   （实际只前 2 个被 service 调用；第 3 个预留 forward-compat）
//! - work_type (4)：`work_type_get_by_id` / `work_type_list_process_ids` /
//!   `work_type_list_work_types_by_process_id` / `work_type_get_max_held_minutes`
//! - process (1)：`process_get_by_id`
//! - process_chain (1)：`process_chain_resolve_step_id_by_process`
//! - part_batch (2)：`part_batch_count_held_by_worker` / `part_batch_get_by_id`
//! - part (3)：`part_get_by_id` / `part_find_inprocess_batch_by_id_and_holder` /
//!   `part_mark_batch_returned`
//! - part_event (1)：`part_insert_part_event`
//! - inline SQL helper (1)：`count_pool_by_shelf_and_process`（service 中
//!   `compute_state` 内的 `sqlx::query_scalar!` 块下沉到本 trait；2026-09-30 起
//!   按 `t_part_batch.current_process_id` 普通过滤，已无 t_process_chain_step JOIN）
//!
//! 2026-10-08 删掉 3 个全仓零调用方：`process_chain_step_get_process_id`
//! （`move_batch` 早已改直读 `batch.current_process_id`）/
//! `part_batch_list_active_by_part_id` / `part_get_process_chain_id`。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::XxxRepo::yyy`。
//! 无需任何 `PgQueueRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockQueueRepoTrait`
//! 供 service 单测注入。queue 域当前无内联 mod tests（service 全部走
//! `tests/queue_api.rs` + `tests/queue_auto_allocate_api.rs` 集成测试守护），
//! 故未建 `service_tests/` 目录——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::modules::part::model::{NewPartEvent, TPart};
use crate::modules::prod::process::model::TProcess;
use crate::modules::prod::work_type::model::TWorkType;
use crate::modules::prod::worker::model::TWorker;
use crate::shared::batch::TPartBatch;

pub mod dispatch;
pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{QueueRepo, TakenRow, CandidateRow}` 这种路径不破（cross-module
// 调用方都依赖这条路径）。
pub use sql::QueueRepo;

use crate::modules::prod::queue::vo::worker::TakenItem;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait QueueRepoTrait: Send {
    // ── t_queue 域（本域 4 方法）──
    async fn take_one_from_pool(
        &mut self,
        worker_id: i64,
        shelf_id: i64,
        process_ids: &[i64],
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, sqlx::Error>;

    /// 单批占坑（`QueueRepo::take_specific_from_pool`）。
    ///
    /// `expected_version` 是**客户端传来的 OCC 锚**（`MoveRequest.version`），
    /// 2026-10-09 起必传：SQL 侧的 `pb.version = candidate.version` 是拿
    /// `FOR UPDATE` 锁住的行比它自己、恒真，吸收并发改动；只有灌进客户端传值
    /// 才是真闸门。`Ok(None)` 兼表「不在候选池」与「version 已变」两种归因，
    /// 由 service 统一转 `40901 VERSION_CONFLICT`。
    async fn take_specific_from_pool(
        &mut self,
        worker_id: i64,
        shelf_id: i64,
        batch_id: i64,
        expected_version: i32,
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, sqlx::Error>;

    /// worker ↔ worker 移动 SQL（2026-09-30 新增）。把 batch 从 src 切到 dst，
    /// 不写 `current_process_step_id`（move 不推进工序链）**也不写
    /// `current_process_id`**（池内移动工序不变 —— 2026-09-30 写入不变式）。
    /// 详见 sql.rs 同名函数。
    async fn move_worker_to_worker(
        &mut self,
        batch_id: i64,
        src_worker_id: i64,
        dst_worker_id: i64,
        expected_version: i32,
        operator_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── worker 域 helper（2）──
    /// 单条查 worker（`WorkerRepo::get_by_id`）。
    async fn worker_get_by_id(
        &mut self,
        worker_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error>;

    /// 按 process 列可执行该工序的 active worker 列表（`WorkerRepo::list_active_by_process_id`）。
    /// 保留 `Result<_, AppError>` 返回类型，与原 `repo.rs` 签名一致。
    async fn worker_list_active_by_process_id(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<(i64, String, i64, String)>, crate::shared::error::AppError>;

    // ── work_type 域 helper（3）──
    /// 单条查 work_type（`WorkTypeRepo::get_by_id`）。
    async fn work_type_get_by_id(&mut self, id: i64) -> Result<Option<TWorkType>, sqlx::Error>;

    /// 列工种映射工序 id 列表（`WorkTypeRepo::list_process_ids`）。
    async fn work_type_list_process_ids(
        &mut self,
        work_type_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error>;

    /// 列工序映射工种列表（`WorkTypeRepo::list_work_types_by_process_id`）。
    /// 保留 `Result<_, AppError>` 返回类型，与原 `repo.rs` 签名一致。
    #[allow(clippy::type_complexity)]
    async fn work_type_list_work_types_by_process_id(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<(i64, String, String, Option<i32>)>, crate::shared::error::AppError>;

    /// 单条查 work_type 的 max_held_minutes（auto-allocate TIME 模式用）。
    /// 原 `service.rs:509` 的 `sqlx::query_as` 内联 SQL 下沉。
    async fn work_type_get_max_held_minutes(&mut self, id: i64)
    -> Result<Option<i32>, sqlx::Error>;

    // ── process 域 helper（1）──
    /// 单条查 process（`ProcessRepo::get_by_id`）。
    async fn process_get_by_id(
        &mut self,
        process_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TProcess>, sqlx::Error>;

    // ── process_chain 域 helper（2）──
    /// 解析 chain 上 process 对应的 step_id（`ProcessChainRepo::resolve_step_id_by_process`）。
    async fn process_chain_resolve_step_id_by_process(
        &mut self,
        chain_id: i64,
        process_id: i64,
    ) -> Result<Option<i64>, sqlx::Error>;

    // ── part_batch 域 helper（2）──
    /// 统计 worker 持有批次数（`PartBatchRepo::count_held_by_worker`）。
    async fn part_batch_count_held_by_worker(&mut self, worker_id: i64)
    -> Result<i64, sqlx::Error>;

    /// 按 id 查 batch（`shared::batch::get_batch_by_id`）。
    async fn part_batch_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartBatch>, sqlx::Error>;

    // ── part 域 helper（3，D-6 未做；直接走 ZST 静态方法）──
    /// 单条查 part（`PartRepo::get_by_id`）。
    async fn part_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error>;

    /// 查 worker 持有中的某 batch（`PartRepo::find_inprocess_batch_by_id_and_holder`）。
    async fn part_find_inprocess_batch_by_id_and_holder(
        &mut self,
        batch_id: i64,
        worker_id: i64,
    ) -> Result<Option<TPartBatch>, sqlx::Error>;

    /// 切 holder 到 shelf（`PartRepo::mark_batch_returned`）。
    ///
    /// 2026-09-30：本转发器**恒传 `None`** 作
    /// `advance_to_process_id` —— move WORKER→POOL 是池内移动、工序不变
    /// （写入不变式），批次归还货架后仍属原工序候选池。推进工序是 worker-scan
    /// RETURNED 的职责，它直连 `PartRepo::mark_batch_returned` 不经本转发器。
    /// `step_id` 形参保持存在（转发给 `current_process_step_id`，被 SQL 丢弃，
    /// 属已知缺口，见 `PartRepo::mark_batch_returned` doc）。
    async fn part_mark_batch_returned(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        step_id: Option<i64>,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── part_event helper（1）──
    /// 写 part 事件日志（`PartRepo::insert_part_event`，PartRepo::insert_part_event
    /// 是 part 域 ZST 静态方法，定义在 `part/repo/event.rs`）。
    #[allow(clippy::too_many_arguments)]
    async fn part_insert_part_event<'a>(
        &mut self,
        id: i64,
        part_id: i64,
        event_type: &'a str,
        from_status: Option<&'a str>,
        to_status: Option<&'a str>,
        batch_id: Option<i64>,
        quantity: Option<i32>,
        drawing_code: Option<&'a str>,
        badge_code: Option<&'a str>,
        note: Option<&'a str>,
        created_by: Option<i64>,
    ) -> Result<(), sqlx::Error>;
}

/// 把 `QueueRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::XxxRepo::yyy`，零转发壳（与
/// iam 2026-09-22 删 `PgIamRepo` 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时
/// `self: &mut &mut PgConnection`，两次 deref 才得到 `PgConnection`：
/// `*self: &mut PgConnection`，`**self: PgConnection`，故喂给
/// `sql::XxxRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl QueueRepoTrait for &mut PgConnection {
    // ── 本域 4 方法 ──
    async fn take_one_from_pool(
        &mut self,
        worker_id: i64,
        shelf_id: i64,
        process_ids: &[i64],
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, sqlx::Error> {
        QueueRepo::take_one_from_pool(
            &mut **self,
            worker_id,
            shelf_id,
            process_ids,
            operator_user_id,
        )
        .await
        .map_err(|e| match e {
            crate::shared::error::AppError::Database(db_err) => db_err,
            other => sqlx::Error::Protocol(other.to_string()),
        })
    }

    async fn take_specific_from_pool(
        &mut self,
        worker_id: i64,
        shelf_id: i64,
        batch_id: i64,
        expected_version: i32,
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, sqlx::Error> {
        QueueRepo::take_specific_from_pool(
            &mut **self,
            worker_id,
            shelf_id,
            batch_id,
            expected_version,
            operator_user_id,
        )
        .await
        .map_err(|e| match e {
            crate::shared::error::AppError::Database(db_err) => db_err,
            other => sqlx::Error::Protocol(other.to_string()),
        })
    }

    async fn move_worker_to_worker(
        &mut self,
        batch_id: i64,
        src_worker_id: i64,
        dst_worker_id: i64,
        expected_version: i32,
        operator_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        QueueRepo::move_worker_to_worker(
            &mut **self,
            batch_id,
            src_worker_id,
            dst_worker_id,
            expected_version,
            operator_user_id,
        )
        .await
        .map_err(|e| match e {
            crate::shared::error::AppError::Database(db_err) => db_err,
            other => sqlx::Error::Protocol(other.to_string()),
        })
    }

    // ── worker helper ──
    async fn worker_get_by_id(
        &mut self,
        worker_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error> {
        crate::modules::prod::worker::repo::WorkerRepo::get_by_id(
            &mut **self,
            worker_id,
            include_deleted,
        )
        .await
    }

    async fn worker_list_active_by_process_id(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<(i64, String, i64, String)>, crate::shared::error::AppError> {
        crate::modules::prod::worker::repo::WorkerRepo::list_active_by_process_id(
            &mut **self,
            process_id,
        )
        .await
    }

    // ── work_type helper ──
    async fn work_type_get_by_id(&mut self, id: i64) -> Result<Option<TWorkType>, sqlx::Error> {
        crate::modules::prod::work_type::repo::WorkTypeRepo::get_by_id(&mut **self, id).await
    }

    async fn work_type_list_process_ids(
        &mut self,
        work_type_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error> {
        crate::modules::prod::work_type::repo::WorkTypeRepo::list_process_ids(
            &mut **self,
            work_type_id,
        )
        .await
    }

    #[allow(clippy::type_complexity)]
    async fn work_type_list_work_types_by_process_id(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<(i64, String, String, Option<i32>)>, crate::shared::error::AppError> {
        Ok(
            crate::modules::prod::work_type::repo::WorkTypeRepo::list_work_types_by_process_id(
                &mut **self,
                process_id,
            )
            .await?,
        )
    }

    async fn work_type_get_max_held_minutes(
        &mut self,
        id: i64,
    ) -> Result<Option<i32>, sqlx::Error> {
        let row: Option<(Option<i32>,)> =
            sqlx::query_as("SELECT max_held_minutes FROM t_work_type WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(row.and_then(|(v,)| v))
    }

    // ── process helper ──
    async fn process_get_by_id(
        &mut self,
        process_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TProcess>, sqlx::Error> {
        crate::modules::prod::process::repo::ProcessRepo::get_by_id(
            &mut **self,
            process_id,
            include_deleted,
        )
        .await
    }

    // ── process_chain helper ──
    async fn process_chain_resolve_step_id_by_process(
        &mut self,
        chain_id: i64,
        process_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        crate::modules::prod::process_chain::repo::ProcessChainRepo::resolve_step_id_by_process(
            &mut **self,
            chain_id,
            process_id,
        )
        .await
    }

    // ── part_batch helper ──
    async fn part_batch_count_held_by_worker(
        &mut self,
        worker_id: i64,
    ) -> Result<i64, sqlx::Error> {
        crate::modules::prod::batch::repo::PartBatchRepo::count_held_by_worker(
            &mut **self,
            worker_id,
        )
        .await
    }

    async fn part_batch_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        crate::shared::batch::get_batch_by_id(&mut **self, id, include_deleted).await
    }

    // ── part helper（直接走 PartRepo ZST 静态方法；D-6 未重构）──
    async fn part_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        use crate::modules::part::repo::PartRepo;
        PartRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn part_find_inprocess_batch_by_id_and_holder(
        &mut self,
        batch_id: i64,
        worker_id: i64,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        use crate::modules::prod::batch::repo::PartBatchRepo;
        PartBatchRepo::find_inprocess_batch_by_id_and_holder(&mut **self, batch_id, worker_id).await
    }

    async fn part_mark_batch_returned(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        step_id: Option<i64>,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        use crate::modules::prod::batch::repo::PartBatchRepo;
        PartBatchRepo::mark_batch_returned(
            &mut **self,
            batch_id,
            expected_version,
            shelf_id,
            step_id,
            // 2026-09-30：move WORKER→POOL = 池内移动，工序不变 →
            // 传 None（= 不推进），SQL 侧 COALESCE 保留原值
            None,
            updated_by,
        )
        .await
    }

    // ── part_event helper（走 PartRepo::insert_part_event）──
    #[allow(clippy::too_many_arguments)]
    async fn part_insert_part_event<'a>(
        &mut self,
        id: i64,
        part_id: i64,
        event_type: &'a str,
        from_status: Option<&'a str>,
        to_status: Option<&'a str>,
        batch_id: Option<i64>,
        quantity: Option<i32>,
        drawing_code: Option<&'a str>,
        badge_code: Option<&'a str>,
        note: Option<&'a str>,
        created_by: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        use crate::modules::part::repo::PartRepo;
        let new = NewPartEvent {
            id,
            part_id,
            event_type,
            from_status,
            to_status,
            batch_id,
            quantity,
            drawing_code,
            badge_code,
            note,
            created_by,
        };
        PartRepo::insert_part_event(&mut **self, new).await
    }
}
