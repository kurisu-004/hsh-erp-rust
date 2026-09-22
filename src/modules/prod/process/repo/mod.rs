//! process 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-2-simple 重构）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，8 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `ProcessRepoTrait`（8 方法合并单 trait），
//!   并直接 `impl ProcessRepoTrait for &mut PgConnection`——handler/service 借
//!   `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `ProcessRepoTrait`（带 `Trait` 后缀）
//! 跨模块静态调用方 `prod::worker_pool/service.rs` /
//! `prod::work_type/service.rs`（2026-09-22 PR6 起，process_mapping 合入 service） /
//! `shelf::service/picker.rs`（注释）/ 共 3+ 处
//! 直接走 ZST 静态方法 `ProcessRepo::xxx(&mut *conn, ...)`，本任务**不能**破坏
//! `prod::process::repo::ProcessRepo` 作为 ZST 的对外身份，故 trait 改名
//! `ProcessRepoTrait`（与 shelf / customer / process_chain / work_type 同形）：
//!
//! - `prod::process::repo::ProcessRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::ProcessRepo;` 重新导出至本模块），保留 8 个 pub 静态方法签名
//!   不变（cross-module 调用方零修改）。
//! - `prod::process::repo::ProcessRepoTrait` —— 本文件新加的胖 trait（8 方法），
//!   process 域内部 service 用 `<R: ProcessRepoTrait>` 收。
//!
//! ## 为什么是胖 trait
//! 与 iam / shelf / customer / process_chain / work_type 范本同形：`&mut PgConnection`
//! 同一作用域只能借给一个 repo 实例；process service 同时需要 `get_by_id` + `update`
//! + `count_process_references` 等多方法才能完成 CRUD，单 trait 一次收下。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::ProcessRepo::yyy`。
//! 无需任何 `PgProcessRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockProcessRepo`
//! 供 service 单测注入。process 域当前无内联 mod tests（service 全部走
//! `tests/process_api.rs` 集成测试守护），故未建 `service_tests/` 目录——按
//! conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1）。

use async_trait::async_trait;
use sqlx::PgConnection;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{TProcess, ProcessRepo}` 这种路径不破（cross-module 调用方都依赖此路径）。
pub use super::model::TProcess;
pub use sql::ProcessRepo;

/// process 域数据访问 trait（8 方法 = t_process 全部读 + 写 + 引用计数）。
///
/// 单 trait 而非按实体拆：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 D-2-simple 重构定案；与
/// iam / shelf / customer / process_chain / work_type 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。
/// `<'a>` 显式生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ProcessRepoTrait: Send {
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TProcess>, sqlx::Error>;
    async fn get_by_code<'a>(
        &mut self,
        code: &'a str,
    ) -> Result<Option<TProcess>, sqlx::Error>;
    async fn list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
    ) -> Result<Vec<TProcess>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn list_with_filters<'a>(
        &mut self,
        code_like: Option<&'a str>,
        category: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TProcess>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &mut self,
        code_like: Option<&'a str>,
        category: Option<&'a str>,
    ) -> Result<i64, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn create<'a>(
        &mut self,
        snowflake_id: i64,
        code: &'a str,
        name: &'a str,
        category: &'a str,
        sort_order: i32,
        description: Option<&'a str>,
        requires_approval: bool,
        color: Option<&'a str>,
        created_by: i64,
    ) -> Result<TProcess, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update<'a>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'a str>,
        sort_order: Option<i32>,
        description: Option<Option<&'a str>>,
        requires_approval: Option<bool>,
        color: Option<Option<&'a str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn count_process_references(
        &mut self,
        process_id: i64,
    ) -> Result<i64, sqlx::Error>;
}

/// 把 `ProcessRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::ProcessRepo::yyy`，零转发壳
/// （与 iam 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::ProcessRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl ProcessRepoTrait for &mut PgConnection {
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TProcess>, sqlx::Error> {
        ProcessRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn get_by_code<'b>(
        &mut self,
        code: &'b str,
    ) -> Result<Option<TProcess>, sqlx::Error> {
        ProcessRepo::get_by_code(&mut **self, code).await
    }

    async fn list_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
    ) -> Result<Vec<TProcess>, sqlx::Error> {
        ProcessRepo::list_by_ids(&mut **self, ids).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_with_filters<'b>(
        &mut self,
        code_like: Option<&'b str>,
        category: Option<&'b str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TProcess>, sqlx::Error> {
        ProcessRepo::list_with_filters(&mut **self, code_like, category, limit, offset).await
    }

    async fn count_with_filters<'b>(
        &mut self,
        code_like: Option<&'b str>,
        category: Option<&'b str>,
    ) -> Result<i64, sqlx::Error> {
        ProcessRepo::count_with_filters(&mut **self, code_like, category).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create<'b>(
        &mut self,
        snowflake_id: i64,
        code: &'b str,
        name: &'b str,
        category: &'b str,
        sort_order: i32,
        description: Option<&'b str>,
        requires_approval: bool,
        color: Option<&'b str>,
        created_by: i64,
    ) -> Result<TProcess, sqlx::Error> {
        ProcessRepo::create(
            &mut **self,
            snowflake_id,
            code,
            name,
            category,
            sort_order,
            description,
            requires_approval,
            color,
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
        sort_order: Option<i32>,
        description: Option<Option<&'b str>>,
        requires_approval: Option<bool>,
        color: Option<Option<&'b str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        ProcessRepo::update(
            &mut **self,
            id,
            version,
            name,
            sort_order,
            description,
            requires_approval,
            color,
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
        ProcessRepo::soft_delete(&mut **self, id, version, updated_by).await
    }

    async fn count_process_references(
        &mut self,
        process_id: i64,
    ) -> Result<i64, sqlx::Error> {
        ProcessRepo::count_process_references(&mut **self, process_id).await
    }
}
