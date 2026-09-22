//! work_type 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-2-simple 重构）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，9 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。其中
//!   `list_work_types_by_process_id` 保留 `Result<_, AppError>` 返回类型，跨模块
//!   静态调用方（`worker_pool::service`）零修改。
//! - `../service.rs`（2026-09-22 PR6 合并入）：`t_work_type_process` SQL 真源（4 静态方法 +
//!   `NewWorkTypeProcessRow`），原 `process_mapping/{mod.rs, sql.rs}` 子目录合并而来。
//! - `mod.rs`（本文件）：对外暴露胖 trait `WorkTypeRepoTrait`（15 方法合并单 trait；
//!   t_work_type 9 + 跨域 helper 2 + t_work_type_process 4），并直接
//!   `impl WorkTypeRepoTrait for &mut PgConnection`——handler/service 借
//!   `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `WorkTypeRepoTrait`（带 `Trait` 后缀）
//! 跨模块静态调用方 `statistics/service.rs` / `prod::worker_pool/service.rs` /
//! `prod::worker/service.rs` / `delivery_note/service/lifecycle.rs` 共 10+ 处直接走
//! ZST 静态方法 `WorkTypeRepo::xxx(&mut *conn, ...)`，本任务**不能**破坏
//! `prod::work_type::repo::WorkTypeRepo` 作为 ZST 的对外身份，故 trait 改名
//! `WorkTypeRepoTrait`（与 shelf / customer / process_chain 同形）：
//!
//! - `prod::work_type::repo::WorkTypeRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::WorkTypeRepo;` 重新导出至本模块），保留 9 个 pub 静态方法签名
//!   不变（cross-module 调用方零修改）。
//! - `prod::work_type::repo::WorkTypeRepoTrait` —— 本文件新加的胖 trait（15 方法），
//!   work_type 域内部 service 用 `<R: WorkTypeRepoTrait>` 收。
//!
//! ## 为什么是胖 trait（合并 t_work_type_process）
//! 与 iam / shelf / customer / process_chain 范本同形：`&mut PgConnection`
//! 同一作用域只能借给一个 repo 实例。决策方案 A（推荐，与 shelf `ShelfRepoTrait`
//! 合并 `t_shelf_process` 同形）—— 单一胖 trait，单 service 签名
//! `<R: WorkTypeRepoTrait>`，单 mock。
//!
//! 备选方案 B（独立 `WorkTypeProcessRepoTrait`）会让 service 收 `<R, M>` 两个泛型 +
//! handler 需两次 `&mut *tx` reborrow，在 `&mut PgConnection` 同一作用域只能借一次
//! 的语义下会撞借用窗口——故未采用。
//!
//! ## 跨域 helper（`process_list_by_ids` / `process_get_by_id`）
//! `WorkTypeProcessService::set_work_type_processes` 校验 items 里的所有
//! `process_id` 存在性。trait impl 一行委托到 `ProcessRepo::xxx` ZST 静态方法。
//!
//! ## t_work_type_process 方法（4）── 用 `worktypeproc_` 前缀消歧义
//! trait 同时承载 `t_work_type_process` 的 4 个方法（与 shelf
//! `ShelfRepoTrait::proc_*` 同形），impl 一行委托到 `super::service::WorkTypeProcessRepo`。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::WorkTypeRepo::yyy`。
//! 无需任何 `PgWorkTypeRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockWorkTypeRepo`
//! 供 service 单测注入。work_type 域当前无内联 mod tests（service 全部走
//! `tests/work_type_api.rs` 集成测试守护），故未建 `service_tests/` 目录——按
//! conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error` 或 `AppError`（保留与原 `repo.rs` 一致的返回类型，
//! 跨模块调用方路径零修改；详见每个方法上的注释）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::prod::process::model::TProcess;
use crate::modules::prod::work_type::service::{NewWorkTypeProcessRow, WorkTypeProcessRepo};

pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{TWorkType, WorkTypeRepo}` 这种路径不破（cross-module 调用方都依赖此路径）。
pub use super::model::TWorkType;
pub use sql::WorkTypeRepo;

/// work_type 域数据访问 trait（15 方法 = t_work_type 9 + 跨域 helper 2 + t_work_type_process 4）。
///
/// 单 trait 而非按实体拆：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 D-2-simple 重构定案；与
/// iam / shelf / customer / process_chain 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。
/// `<'a>` 显式生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait WorkTypeRepoTrait: Send {
    // ── t_work_type（9）──
    async fn get_by_id(&mut self, id: i64) -> Result<Option<TWorkType>, sqlx::Error>;
    async fn get_by_code<'a>(
        &mut self,
        code: &'a str,
    ) -> Result<Option<TWorkType>, sqlx::Error>;
    async fn list_process_ids(
        &mut self,
        work_type_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error>;
    async fn list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
    ) -> Result<Vec<TWorkType>, sqlx::Error>;
    async fn list_with_filters<'a>(
        &mut self,
        code_like: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TWorkType>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &mut self,
        code_like: Option<&'a str>,
    ) -> Result<i64, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn create<'a>(
        &mut self,
        snowflake_id: i64,
        code: &'a str,
        name: &'a str,
        description: Option<&'a str>,
        sort_order: i32,
        max_held_batches: Option<i32>,
        created_by: i64,
    ) -> Result<TWorkType, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update<'a>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'a str>,
        description: Option<Option<&'a str>>,
        sort_order: Option<i32>,
        max_held_batches: Option<Option<i32>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn count_work_type_references(
        &mut self,
        work_type_id: i64,
    ) -> Result<i64, sqlx::Error>;

    /// 保留 `Result<_, AppError>` 返回类型（与原 `repo.rs` 一致），让跨模块静态
    /// 调用方 `prod::worker_pool::service` 走 ZST 调用路径时 `?` 自动转换。
    #[allow(clippy::type_complexity)]
    async fn list_work_types_by_process_id(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<(i64, String, String, Option<i32>)>, crate::shared::error::AppError>;

    // ── 跨域 helper（2）── 委托到 process 域 ZST 静态方法 ────────────
    /// 单条查 process（活跃行）。供本域 `WorkTypeProcessService::set_work_type_processes`
    /// 校验 `process_id` 存在性。
    async fn process_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<TProcess>, sqlx::Error>;
    /// 批量查 process（活跃行）。空切片短路返回空 Vec。供本域
    /// `WorkTypeProcessService::set_work_type_processes` 一次性校验 items 里所有
    /// `process_id` 存在性，防 N+1。
    async fn process_list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
    ) -> Result<Vec<TProcess>, sqlx::Error>;

    // ── t_work_type_process（4）── 用 worktypeproc_ 前缀消歧义 ─────
    /// 按 `work_type_id` 取所有 active mapping（按 sort_order ASC）。
    async fn worktypeproc_list_by_work_type(
        &mut self,
        work_type_id: i64,
    ) -> Result<Vec<(i64, i32, String)>, sqlx::Error>;
    /// 批量取一组工种的 process_id 列表（防 N+1）。
    async fn worktypeproc_list_by_work_types_batch<'a>(
        &mut self,
        work_type_ids: &'a [i64],
    ) -> Result<Vec<(i64, i64)>, sqlx::Error>;
    /// 软删一个 work_type 的全部 active mapping。
    async fn worktypeproc_soft_delete_all_for_work_type(
        &mut self,
        work_type_id: i64,
    ) -> Result<u64, sqlx::Error>;
    /// 批量插入新 mapping：单条 INSERT ... VALUES (...), (...), (...)。
    async fn worktypeproc_bulk_insert(
        &mut self,
        rows: &[NewWorkTypeProcessRow],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error>;
}

/// 把 `WorkTypeRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::WorkTypeRepo::yyy`，零转发壳
/// （与 iam 2026-09-22 删 `PgIamRepo` 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::WorkTypeRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl WorkTypeRepoTrait for &mut PgConnection {
    // ── t_work_type（9）── 一行委托 sql::WorkTypeRepo ───────────────
    async fn get_by_id(&mut self, id: i64) -> Result<Option<TWorkType>, sqlx::Error> {
        WorkTypeRepo::get_by_id(&mut **self, id).await
    }

    async fn get_by_code<'b>(
        &mut self,
        code: &'b str,
    ) -> Result<Option<TWorkType>, sqlx::Error> {
        WorkTypeRepo::get_by_code(&mut **self, code).await
    }

    async fn list_process_ids(&mut self, work_type_id: i64) -> Result<Vec<i64>, sqlx::Error> {
        WorkTypeRepo::list_process_ids(&mut **self, work_type_id).await
    }

    async fn list_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
    ) -> Result<Vec<TWorkType>, sqlx::Error> {
        WorkTypeRepo::list_by_ids(&mut **self, ids).await
    }

    async fn list_with_filters<'b>(
        &mut self,
        code_like: Option<&'b str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TWorkType>, sqlx::Error> {
        WorkTypeRepo::list_with_filters(&mut **self, code_like, limit, offset).await
    }

    async fn count_with_filters<'b>(
        &mut self,
        code_like: Option<&'b str>,
    ) -> Result<i64, sqlx::Error> {
        WorkTypeRepo::count_with_filters(&mut **self, code_like).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create<'b>(
        &mut self,
        snowflake_id: i64,
        code: &'b str,
        name: &'b str,
        description: Option<&'b str>,
        sort_order: i32,
        max_held_batches: Option<i32>,
        created_by: i64,
    ) -> Result<TWorkType, sqlx::Error> {
        WorkTypeRepo::create(
            &mut **self,
            snowflake_id,
            code,
            name,
            description,
            sort_order,
            max_held_batches,
            created_by,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn update<'b>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'b str>,
        description: Option<Option<&'b str>>,
        sort_order: Option<i32>,
        max_held_batches: Option<Option<i32>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        WorkTypeRepo::update(
            &mut **self,
            id,
            version,
            name,
            description,
            sort_order,
            max_held_batches,
            updated_by,
        )
        .await
    }

    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        WorkTypeRepo::soft_delete(&mut **self, id, version, updated_by).await
    }

    async fn count_work_type_references(
        &mut self,
        work_type_id: i64,
    ) -> Result<i64, sqlx::Error> {
        WorkTypeRepo::count_work_type_references(&mut **self, work_type_id).await
    }

    async fn list_work_types_by_process_id(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<(i64, String, String, Option<i32>)>, crate::shared::error::AppError> {
        WorkTypeRepo::list_work_types_by_process_id(&mut **self, process_id).await
    }

    // ── 跨域 helper（2）── 一行委托 process 域 ZST 静态方法 ─────────
    async fn process_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<TProcess>, sqlx::Error> {
        use crate::modules::prod::process::repo::ProcessRepo;
        ProcessRepo::get_by_id(&mut **self, id, false).await
    }

    async fn process_list_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
    ) -> Result<Vec<TProcess>, sqlx::Error> {
        use crate::modules::prod::process::repo::ProcessRepo;
        ProcessRepo::list_by_ids(&mut **self, ids).await
    }

    // ── t_work_type_process（4）── 一行委托 super::service::WorkTypeProcessRepo ─
    async fn worktypeproc_list_by_work_type(
        &mut self,
        work_type_id: i64,
    ) -> Result<Vec<(i64, i32, String)>, sqlx::Error> {
        WorkTypeProcessRepo::list_by_work_type(&mut **self, work_type_id).await
    }

    async fn worktypeproc_list_by_work_types_batch<'b>(
        &mut self,
        work_type_ids: &'b [i64],
    ) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        WorkTypeProcessRepo::list_by_work_types_batch(&mut **self, work_type_ids).await
    }

    async fn worktypeproc_soft_delete_all_for_work_type(
        &mut self,
        work_type_id: i64,
    ) -> Result<u64, sqlx::Error> {
        WorkTypeProcessRepo::soft_delete_all_for_work_type(&mut **self, work_type_id).await
    }

    async fn worktypeproc_bulk_insert(
        &mut self,
        rows: &[NewWorkTypeProcessRow],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error> {
        WorkTypeProcessRepo::bulk_insert(&mut **self, rows, snowflake, created_by).await
    }
}
