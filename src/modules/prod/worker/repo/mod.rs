//! worker 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-2-simple 重构）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，9 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。其中
//!   `list_active_by_process_id` 保留 `Result<_, AppError>` 返回类型，跨模块
//!   静态调用方（`worker_pool::service`）零修改。
//! - `mod.rs`（本文件）：对外暴露胖 trait `WorkerRepoTrait`（11 方法合并单 trait；
//!   t_worker 9 + 跨域 helper 2），并直接 `impl WorkerRepoTrait for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `WorkerRepoTrait`（带 `Trait` 后缀）
//! 跨模块静态调用方 `_e2e/handler.rs` / `statistics/service.rs` /
//! `prod::worker_pool/service.rs` / `part/service/{worker_scan,phase1}.rs` /
//! `delivery_note/service/lifecycle.rs` 共 17+ 处直接走 ZST 静态方法
//! `WorkerRepo::xxx(&mut *conn, ...)`，本任务**不能**破坏
//! `prod::worker::repo::WorkerRepo` 作为 ZST 的对外身份，故 trait 改名
//! `WorkerRepoTrait`（与 shelf / customer / process_chain 同形）：
//!
//! - `prod::worker::repo::WorkerRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::WorkerRepo;` 重新导出至本模块），保留 9 个 pub 静态方法签名
//!   不变（cross-module 调用方零修改）。
//! - `prod::worker::repo::WorkerRepoTrait` —— 本文件新加的胖 trait（11 方法），
//!   worker 域内部 service 用 `<R: WorkerRepoTrait>` 收。
//!
//! ## 为什么是胖 trait
//! 与 iam / shelf / customer / process_chain 范本同形：`&mut PgConnection`
//! 同一作用域只能借给一个 repo 实例；worker service 同时需要 `get_by_id` + `update`
//! + `count_in_use_parts` 等多方法才能完成 CRUD，单 trait 一次收下。
//!
//! ## 跨域 helper（`work_type_get_by_id` / `work_type_list_by_ids`）
//! worker 域 service 需要补齐 `work_type.name`（list 批量 + get/create/update 单条）。
//! 与 shelf 同形：跨域 helper 在 trait 上声明，trait impl 一行委托到
//! `prod::work_type::repo::WorkTypeRepo::xxx` 静态方法。service 拿 trait 即可，
//! 零 inline 跨域 SQL。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::WorkerRepo::yyy`。
//! 无需任何 `PgWorkerRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockWorkerRepo`
//! 供 service 单测注入。worker 域当前无内联 mod tests（service 全部走
//! `tests/worker_api.rs` 集成测试守护），故未建 `service_tests/` 目录——按
//! conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error` 或 `AppError`（保留与原 `repo.rs` 一致的返回类型，
//! 跨模块调用方路径零修改；详见每个方法上的注释）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::modules::prod::work_type::model::TWorkType;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{TWorker, WorkerRepo}` 这种路径不破（cross-module 调用方都依赖此路径）。
pub use super::model::TWorker;
pub use sql::WorkerRepo;

/// worker 域数据访问 trait（11 方法 = t_worker 9 + 跨域 helper 2）。
///
/// 单 trait 而非按实体拆：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 D-2-simple 重构定案；与
/// iam / shelf / customer / process_chain 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。
/// `<'a>` 显式生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait WorkerRepoTrait: Send {
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error>;
    async fn get_by_badge_code<'a>(
        &mut self,
        badge_code: &'a str,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error>;
    async fn list_with_filters<'a>(
        &mut self,
        name_like: Option<&'a str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TWorker>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &mut self,
        name_like: Option<&'a str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn create<'a>(
        &mut self,
        snowflake_id: i64,
        badge_code: &'a str,
        name: &'a str,
        id_card_no: Option<&'a str>,
        phone: Option<&'a str>,
        work_type_id: Option<i64>,
        created_by: i64,
    ) -> Result<TWorker, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update<'a>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'a str>,
        badge_code: Option<&'a str>,
        id_card_no: Option<Option<&'a str>>,
        phone: Option<Option<&'a str>>,
        work_type_id: Option<Option<i64>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn deactivate(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn reactivate(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn count_in_use_parts(&mut self, worker_id: i64) -> Result<i64, sqlx::Error>;

    /// 保留 `Result<_, AppError>` 返回类型（与原 `repo.rs` 一致），让跨模块静态
    /// 调用方 `prod::worker_pool::service` 走 ZST 调用路径时 `?` 自动转换。
    async fn list_active_by_process_id(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<(i64, String, i64, String)>, crate::shared::error::AppError>;

    // ── 跨域 helper（2）── 委托到 work_type 域 ZST 静态方法 ───────────
    /// 单条查 work_type（活跃行）。供本域 `get_worker` / `create_worker` /
    /// `update_worker` 校验 `work_type_id` 存在性 + 补 `work_type_name`。
    async fn work_type_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<TWorkType>, sqlx::Error>;
    /// 批量查 work_type（活跃行）。空切片短路返回空 Vec。供本域 `list_workers`
    /// 一次性补齐 `work_type_name`，防 N+1。
    async fn work_type_list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
    ) -> Result<Vec<TWorkType>, sqlx::Error>;
}

/// 把 `WorkerRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借 `&mut *tx` 或
/// `&mut *conn` 即可调用 `sql::WorkerRepo::yyy`，零转发壳（与 iam 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::WorkerRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl WorkerRepoTrait for &mut PgConnection {
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error> {
        WorkerRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn get_by_badge_code<'b>(
        &mut self,
        badge_code: &'b str,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error> {
        WorkerRepo::get_by_badge_code(&mut **self, badge_code, include_deleted).await
    }

    async fn list_with_filters<'b>(
        &mut self,
        name_like: Option<&'b str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TWorker>, sqlx::Error> {
        WorkerRepo::list_with_filters(&mut **self, name_like, is_active, limit, offset).await
    }

    async fn count_with_filters<'b>(
        &mut self,
        name_like: Option<&'b str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        WorkerRepo::count_with_filters(&mut **self, name_like, is_active).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create<'b>(
        &mut self,
        snowflake_id: i64,
        badge_code: &'b str,
        name: &'b str,
        id_card_no: Option<&'b str>,
        phone: Option<&'b str>,
        work_type_id: Option<i64>,
        created_by: i64,
    ) -> Result<TWorker, sqlx::Error> {
        WorkerRepo::create(
            &mut **self,
            snowflake_id,
            badge_code,
            name,
            id_card_no,
            phone,
            work_type_id,
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
        badge_code: Option<&'b str>,
        id_card_no: Option<Option<&'b str>>,
        phone: Option<Option<&'b str>>,
        work_type_id: Option<Option<i64>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        WorkerRepo::update(
            &mut **self,
            id,
            version,
            name,
            badge_code,
            id_card_no,
            phone,
            work_type_id,
            updated_by,
        )
        .await
    }

    async fn deactivate(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        WorkerRepo::deactivate(&mut **self, id, version, updated_by).await
    }

    async fn reactivate(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        WorkerRepo::reactivate(&mut **self, id, version, updated_by).await
    }

    async fn count_in_use_parts(&mut self, worker_id: i64) -> Result<i64, sqlx::Error> {
        WorkerRepo::count_in_use_parts(&mut **self, worker_id).await
    }

    async fn list_active_by_process_id(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<(i64, String, i64, String)>, crate::shared::error::AppError> {
        WorkerRepo::list_active_by_process_id(&mut **self, process_id).await
    }

    // ── 跨域 helper（2）── 一行委托 work_type 域 ZST 静态方法 ──────
    async fn work_type_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<TWorkType>, sqlx::Error> {
        use crate::modules::prod::work_type::repo::WorkTypeRepo;
        WorkTypeRepo::get_by_id(&mut **self, id).await
    }

    async fn work_type_list_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
    ) -> Result<Vec<TWorkType>, sqlx::Error> {
        use crate::modules::prod::work_type::repo::WorkTypeRepo;
        WorkTypeRepo::list_by_ids(&mut **self, ids).await
    }
}
