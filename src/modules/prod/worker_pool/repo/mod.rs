//! worker_pool 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-2 重构对齐 iam / shelf / customer / worker 范本）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，4 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `WorkerPoolRepoTrait`（本域 4 方法 + 跨域
//!   helper 14 方法），并直接 `impl WorkerPoolRepoTrait for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `WorkerPoolRepoTrait`（带 `Trait` 后缀）
//! 本任务范围内 `_e2e` / `statistics` / 其他域对 worker_pool repo 的跨模块静态调用
//! 命中数为 0（grep 校验：除 `src/modules/prod/worker_pool/` 自身外无
//! `WorkerPoolRepo::` 调用），但仍按 shelf / customer / process_chain / worker 范本
//! 命名 `*Trait` 后缀——未来跨模块调用方零修改成本（trait 改名比 ZST 改名风险低）。
//!
//! - `prod::worker_pool::repo::WorkerPoolRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::WorkerPoolRepo;` 重新导出至本模块），保留 4 个 pub 静态方法签名
//!   不变（任何未来 cross-module 调用方零修改）。
//! - `prod::worker_pool::repo::WorkerPoolRepoTrait` —— 本文件新加的胖 trait
//!   （18 方法 = 本域 4 + 跨域 helper 14），worker_pool 域内部 service 用
//!   `<R: WorkerPoolRepoTrait>` 收。
//!
//! ## 为什么是胖 trait（含本域 + 跨域 helper）
//! worker_pool 是跨域核心（CLAUDE.md §「part 是跨域枢纽」的兄弟节点）——同 service
//! 方法（`refill_for_worker_with_work_type` / `compute_state` / `admin_remove_held_batch` /
//! `auto_allocate_for_process` / `assign_batch_to_worker`）需交替访问
//! t_worker_pool（本域）+ t_worker + t_work_type + t_process + t_process_chain_step +
//! t_part_batch + t_part 共 7 表。
//!
//! 与 assembly / shelf / process_chain 同形：跨域 SQL 全部下沉到 `WorkerPoolRepoTrait`
//! helper 方法，trait impl 一行委托到对应 ZST 静态方法（已重构域）或原 ZST 静态方法
//! （part 域，D-6 未做）。
//!
//! ## 跨域 helper 清单（14）
//! - worker (3)：`worker_get_by_id` / `worker_list_active_by_process_id` /
//!   `worker_list_with_filters_for_seed`
//!   （实际只前 2 个被 service 调用；第 3 个预留 forward-compat）
//! - work_type (3)：`work_type_get_by_id` / `work_type_list_process_ids` /
//!   `work_type_list_work_types_by_process_id`
//! - process (1)：`process_get_by_id`
//! - process_chain (1)：`process_chain_resolve_step_id_by_process`
//! - process_chain_step (1)：`process_chain_step_get_process_id`（assign 路径校验用）
//! - part_batch (3)：`part_batch_count_held_by_worker` / `part_batch_get_by_id` /
//!   `part_batch_list_active_by_part_id`
//! - part (3)：`part_get_by_id` / `part_find_inprocess_batch_by_id_and_holder` /
//!   `part_mark_batch_returned`
//! - part_event (1)：`part_insert_part_event`
//! - inline SQL helper (1)：`count_pool_by_shelf_and_process`（service 中
//!   `compute_state` 内的 `sqlx::query_scalar!` 块下沉到本 trait；t_part_batch +
//!   t_process_chain_step JOIN）
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::XxxRepo::yyy`。
//! 无需任何 `PgWorkerPoolRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockWorkerPoolRepoTrait`
//! 供 service 单测注入。worker_pool 域当前无内联 mod tests（service 全部走
//! `tests/worker_pool_api.rs` + `tests/worker_pool_auto_allocate_api.rs` 集成测试守护），
//! 故未建 `service_tests/` 目录——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::modules::part::model::{NewPartEvent, TPart};
use crate::modules::part_batch::model::TPartBatch;
use crate::modules::prod::process::model::TProcess;
use crate::modules::prod::work_type::model::TWorkType;
use crate::modules::prod::worker::model::TWorker;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{WorkerPoolRepo, TakenRow, CandidateRow}` 这种路径不破（cross-module
// 调用方都依赖这条路径）。
pub use sql::WorkerPoolRepo;

use crate::modules::prod::worker_pool::model::TakenItem;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait WorkerPoolRepoTrait: Send {
    // ── t_worker_pool 域（本域 4 方法）──
    async fn take_one_from_pool(
        &mut self,
        worker_id: i64,
        shelf_id: i64,
        process_ids: &[i64],
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, sqlx::Error>;

    async fn take_specific_from_pool(
        &mut self,
        worker_id: i64,
        shelf_id: i64,
        batch_id: i64,
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, sqlx::Error>;

    async fn list_candidates_by_process_all_shelves(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<super::dto::PoolBatchItem>, sqlx::Error>;

    async fn list_held_by_worker_with_part(
        &mut self,
        worker_id: i64,
    ) -> Result<Vec<super::model::HeldBatchItem>, sqlx::Error>;

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
    async fn work_type_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<TWorkType>, sqlx::Error>;

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
    async fn work_type_get_max_held_minutes(
        &mut self,
        id: i64,
    ) -> Result<Option<i32>, sqlx::Error>;

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

    /// 解析 step_id 对应的 process_id（assign 路径校验 batch 当前 step）。
    /// 原 `service.rs:739` 的 `sqlx::query_scalar` 内联 SQL 下沉。
    async fn process_chain_step_get_process_id(
        &mut self,
        step_id: i64,
    ) -> Result<Option<i64>, sqlx::Error>;

    // ── part_batch 域 helper（3）──
    /// 统计 worker 持有批次数（`PartBatchRepo::count_held_by_worker`）。
    async fn part_batch_count_held_by_worker(
        &mut self,
        worker_id: i64,
    ) -> Result<i64, sqlx::Error>;

    /// 按 id 查 batch（`PartBatchRepo::get_by_id`）。
    async fn part_batch_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartBatch>, sqlx::Error>;

    /// 列 part 全部活跃批次（`PartBatchRepo::list_active_by_part_id`，sync_from_batch_change
    /// 间接消费：service 直接调 `PartService::sync_from_batch_change` 不走本 trait）。
    /// 预留以便后续 service 重构时下沉到 trait。
    #[allow(dead_code)]
    async fn part_batch_list_active_by_part_id(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error>;

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

    /// 切 holder 到 shelf + 改 step_id（`PartRepo::mark_batch_returned`）。
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

    // ── 跨域 inline SQL helper（1，service `compute_state` 内联下沉）──
    /// 池候选数（按 shelf + process 维度）。
    /// 原 `service.rs:225` 的 `sqlx::query_scalar!` 内联 SQL 下沉。
    async fn count_pool_by_shelf_and_process(
        &mut self,
        shelf_id: i64,
        process_id: i64,
    ) -> Result<i64, sqlx::Error>;

    // ── 跨域 inline SQL helper（1，admin_remove 内联下沉）──
    /// 取 part 的 process_chain_id（admin_remove 路径解析 step 用）。
    /// 原 `service.rs:304` 的 `sqlx::query_scalar` 内联 SQL 下沉。
    async fn part_get_process_chain_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<i64>, sqlx::Error>;
}

/// 把 `WorkerPoolRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::XxxRepo::yyy`，零转发壳（与
/// iam 2026-09-22 删 `PgIamRepo` 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时
/// `self: &mut &mut PgConnection`，两次 deref 才得到 `PgConnection`：
/// `*self: &mut PgConnection`，`**self: PgConnection`，故喂给
/// `sql::XxxRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl WorkerPoolRepoTrait for &mut PgConnection {
    // ── 本域 4 方法 ──
    async fn take_one_from_pool(
        &mut self,
        worker_id: i64,
        shelf_id: i64,
        process_ids: &[i64],
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, sqlx::Error> {
        WorkerPoolRepo::take_one_from_pool(
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
        operator_user_id: i64,
    ) -> Result<Option<TakenItem>, sqlx::Error> {
        WorkerPoolRepo::take_specific_from_pool(
            &mut **self,
            worker_id,
            shelf_id,
            batch_id,
            operator_user_id,
        )
        .await
        .map_err(|e| match e {
            crate::shared::error::AppError::Database(db_err) => db_err,
            other => sqlx::Error::Protocol(other.to_string()),
        })
    }

    async fn list_candidates_by_process_all_shelves(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<super::dto::PoolBatchItem>, sqlx::Error> {
        WorkerPoolRepo::list_candidates_by_process_all_shelves(&mut **self, process_id)
            .await
            .map_err(|e| match e {
                crate::shared::error::AppError::Database(db_err) => db_err,
                other => sqlx::Error::Protocol(other.to_string()),
            })
    }

    async fn list_held_by_worker_with_part(
        &mut self,
        worker_id: i64,
    ) -> Result<Vec<super::model::HeldBatchItem>, sqlx::Error> {
        WorkerPoolRepo::list_held_by_worker_with_part(&mut **self, worker_id).await
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
    async fn work_type_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<TWorkType>, sqlx::Error> {
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
        Ok(crate::modules::prod::work_type::repo::WorkTypeRepo::list_work_types_by_process_id(
            &mut **self,
            process_id,
        )
        .await?)
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

    async fn process_chain_step_get_process_id(
        &mut self,
        step_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<i64> = sqlx::query_scalar(
            "SELECT process_id FROM t_process_chain_step \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(step_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row)
    }

    // ── part_batch helper ──
    async fn part_batch_count_held_by_worker(
        &mut self,
        worker_id: i64,
    ) -> Result<i64, sqlx::Error> {
        crate::modules::part_batch::repo::PartBatchRepo::count_held_by_worker(&mut **self, worker_id)
            .await
    }

    async fn part_batch_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartBatch>, sqlx::Error> {
        crate::modules::part_batch::repo::PartBatchRepo::get_by_id(&mut **self, id, include_deleted)
            .await
    }

    #[allow(dead_code)]
    async fn part_batch_list_active_by_part_id(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        crate::modules::part_batch::repo::PartBatchRepo::list_active_by_part_id(
            &mut **self,
            part_id,
        )
        .await
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
        use crate::modules::part::repo::PartRepo;
        PartRepo::find_inprocess_batch_by_id_and_holder(&mut **self, batch_id, worker_id).await
    }

    async fn part_mark_batch_returned(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        step_id: Option<i64>,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        use crate::modules::part::repo::PartRepo;
        PartRepo::mark_batch_returned(
            &mut **self,
            batch_id,
            expected_version,
            shelf_id,
            step_id,
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

    // ── 跨域 inline SQL helper（compute_state 路径）──
    async fn count_pool_by_shelf_and_process(
        &mut self,
        shelf_id: i64,
        process_id: i64,
    ) -> Result<i64, sqlx::Error> {
        // PR-3 批次 step 化：next_process_id 列已删，JOIN step 取 process_id
        let n: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!" FROM t_part_batch pb
            JOIN t_process_chain_step s ON s.id = pb.current_process_step_id
            WHERE pb.status = 'IN_PROCESS'
              AND pb.location = 'PRODUCTION_SHELF'
              AND pb.current_holder_id = $1
              AND s.process_id = $2
              AND pb.deleted_at IS NULL
              AND s.deleted_at IS NULL"#,
            shelf_id,
            process_id
        )
        .fetch_one(&mut **self)
        .await?;
        Ok(n)
    }

    // ── 跨域 inline SQL helper（admin_remove 路径）──
    async fn part_get_process_chain_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<i64> = sqlx::query_scalar(
            "SELECT process_chain_id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row)
    }
}
