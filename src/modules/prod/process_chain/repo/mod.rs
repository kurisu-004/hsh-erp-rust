//! process_chain 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-1 重构对齐 iam / shelf / customer 范本）
//! - `sql.rs`：原 `repo/mod.rs` 内的 ZST struct `ProcessChainRepo` 单独抽到本文件；
//!   `pub use sql::ProcessChainRepo;` 重导出至本模块，跨模块静态调用方路径零修改。
//! - `mutate.rs`：写路径（INSERT/UPDATE/DELETE）impl `ProcessChainRepo` 块；
//!   2026-09-22 抽 trait 时 `use super::ProcessChainRepo;` 改为 `use super::sql::ProcessChainRepo;`，
//!   SQL 字符串零 diff（与 `query.rs` 同形）；`bulk_insert_steps` 签名由
//!   `&mut PgConnection` 改为 `impl PgExecutor<'_>`（与 shelf `bulk_insert` 同形），
//!   SQL 字符串零 diff。
//! - `query.rs`：读路径（SELECT/JOIN）impl `ProcessChainRepo` 块；同上，零 diff。
//! - `mod.rs`（本文件）：对外暴露胖 trait `ProcessChainRepoTrait`（10 方法合并单 trait；
//!   t_part_process_chain 5 + t_process_chain_step 4 + 跨 step 解析 1），并直接
//!   `impl ProcessChainRepoTrait for &mut PgConnection`——handler/service 借
//!   `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `ProcessChainRepoTrait`（带 `Trait` 后缀）
//! 跨模块静态调用方 11 处直接走 ZST 静态方法（part 域 9 + worker_pool 域 1 +
//! part/service/inspection_core 1），本任务**不能**破坏
//! `prod::process_chain::repo::ProcessChainRepo` 作为 ZST 的对外身份，故 trait
//! 改名 `ProcessChainRepoTrait`（与 shelf / customer 范本同形）：
//!
//! - `prod::process_chain::repo::ProcessChainRepo` —— ZST struct（在 `sql.rs` 内，
//!   通过 `pub use sql::ProcessChainRepo;` 重新导出至本模块），保留 10 个 pub 静态
//!   方法签名不变（cross-module 调用方零修改）。
//! - `prod::process_chain::repo::ProcessChainRepoTrait` —— 本文件新加的胖 trait，
//!   process_chain 域内部 service 用 `<R: ProcessChainRepoTrait>` 收。
//!
//! ## 为什么是胖 trait
//! 与 iam / shelf / customer 范本同形：`&mut PgConnection` 同一作用域只能借给一
//! 个 repo 实例；process_chain service 同时需要 `get_chain_by_part` +
//! `bump_chain_version` + `bulk_insert_steps`（来自 sql.rs / mutate.rs / query.rs
//! 三处）才能完成 upsert，单 trait 一次收下。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::ProcessChainRepo::yyy`。
//! 无需任何 `PgProcessChainRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockProcessChainRepo`
//! 供 service 单测注入。process_chain 域当前无内联 mod tests（service 全部走
//! `tests/process_chain_api.rs` 集成测试守护），故未建 `service_tests/` 目录
//! ——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs / mutate.rs / query.rs 签名 1:1）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::infra::snowflake::SnowflakeIdGenerator;

pub mod mutate;
pub mod query;
pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{TPartProcessChain, TProcessChainStep, NewProcessChainStep, ProcessChainRepo}`
// 这种路径不破（cross-module 调用方都依赖这条路径）。
pub use super::model::{NewProcessChainStep, TPartProcessChain, TProcessChainStep};
pub use sql::ProcessChainRepo;

/// process_chain 域数据访问 trait（10 方法 = t_part_process_chain 4 + t_process_chain_step 5 + 跨 1）。
///
/// 单 trait 而非每表一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 D-1 重构定案；与 iam / shelf /
/// customer 同形）。
///
/// 方法签名 = `sql.rs` / `mutate.rs` / `query.rs` 固有静态方法去 executor 形参。
/// `<'a>` 显式生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ProcessChainRepoTrait: Send {
    // ── t_part_process_chain（4）── 来自 mutate.rs + query.rs
    async fn insert_chain<'a>(
        &mut self,
        id: i64,
        name: &'a str,
        note: Option<&'a str>,
        created_by: i64,
    ) -> Result<TPartProcessChain, sqlx::Error>;
    async fn link_chain_to_part(
        &mut self,
        part_id: i64,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete_chain(
        &mut self,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn unlink_part_from_chain(
        &mut self,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn bump_chain_version<'a>(
        &mut self,
        chain_id: i64,
        expected_version: i32,
        name: Option<&'a str>,
        note: Option<Option<&'a str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete_all_steps_for_chain(
        &mut self,
        chain_id: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn get_chain_by_part(
        &mut self,
        part_id: i64,
    ) -> Result<Option<TPartProcessChain>, sqlx::Error>;
    async fn get_chain_by_id(
        &mut self,
        chain_id: i64,
    ) -> Result<Option<TPartProcessChain>, sqlx::Error>;

    // ── t_process_chain_step（2）── 来自 query.rs
    async fn list_steps_by_chain(
        &mut self,
        chain_id: i64,
    ) -> Result<Vec<TProcessChainStep>, sqlx::Error>;
    async fn next_step_in_chain(
        &mut self,
        chain_id: i64,
        current_sort_order: i32,
    ) -> Result<Option<TProcessChainStep>, sqlx::Error>;

    // ── 跨 step 解析（1）── 来自 query.rs（PR-3 批次 step 化新增）
    async fn resolve_step_id_by_process(
        &mut self,
        chain_id: i64,
        process_id: i64,
    ) -> Result<Option<i64>, sqlx::Error>;

    // ── 批量 INSERT steps（1）── 来自 mutate.rs（与 shelf bulk_insert 同形）
    async fn bulk_insert_steps<'a>(
        &mut self,
        chain_id: i64,
        rows: &'a [NewProcessChainStep],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── 跨域 helper（1）── 委托到 part 域 PartRepo 静态方法，避免 service 收第二个 conn
    /// 检查 part 是否存在（`PartRepo::get_by_id(id, false).await?.is_some()`）。
    /// 用于 `upsert_chain` 校验 part 存在性 + PENDING 状态。
    async fn part_get_by_id(
        &mut self,
        part_id: i64,
        include_deleted: bool,
    ) -> Result<Option<crate::modules::part::model::TPart>, sqlx::Error>;
}

/// 把 `ProcessChainRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::ProcessChainRepo::yyy`，零转发壳
/// （与 iam 2026-09-22 删 `PgIamRepo` 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时
/// `self: &mut &mut PgConnection`，两次 deref 才得到 `PgConnection`：
/// `*self: &mut PgConnection`，`**self: PgConnection`，故喂给
/// `sql::ProcessChainRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl ProcessChainRepoTrait for &mut PgConnection {
    // ── mutate.rs（5）──
    async fn insert_chain<'b>(
        &mut self,
        id: i64,
        name: &'b str,
        note: Option<&'b str>,
        created_by: i64,
    ) -> Result<TPartProcessChain, sqlx::Error> {
        ProcessChainRepo::insert_chain(&mut **self, id, name, note, created_by).await
    }

    async fn link_chain_to_part(
        &mut self,
        part_id: i64,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        ProcessChainRepo::link_chain_to_part(&mut **self, part_id, chain_id, updated_by).await
    }

    async fn soft_delete_chain(
        &mut self,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        ProcessChainRepo::soft_delete_chain(&mut **self, chain_id, updated_by).await
    }

    async fn unlink_part_from_chain(
        &mut self,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        ProcessChainRepo::unlink_part_from_chain(&mut **self, chain_id, updated_by).await
    }

    async fn bump_chain_version<'b>(
        &mut self,
        chain_id: i64,
        expected_version: i32,
        name: Option<&'b str>,
        note: Option<Option<&'b str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        ProcessChainRepo::bump_chain_version(
            &mut **self,
            chain_id,
            expected_version,
            name,
            note,
            updated_by,
        )
        .await
    }

    async fn soft_delete_all_steps_for_chain(
        &mut self,
        chain_id: i64,
    ) -> Result<u64, sqlx::Error> {
        ProcessChainRepo::soft_delete_all_steps_for_chain(&mut **self, chain_id).await
    }

    // ── query.rs（4）──
    async fn get_chain_by_part(
        &mut self,
        part_id: i64,
    ) -> Result<Option<TPartProcessChain>, sqlx::Error> {
        ProcessChainRepo::get_chain_by_part(&mut **self, part_id).await
    }

    async fn get_chain_by_id(
        &mut self,
        chain_id: i64,
    ) -> Result<Option<TPartProcessChain>, sqlx::Error> {
        ProcessChainRepo::get_chain_by_id(&mut **self, chain_id).await
    }

    async fn list_steps_by_chain(
        &mut self,
        chain_id: i64,
    ) -> Result<Vec<TProcessChainStep>, sqlx::Error> {
        ProcessChainRepo::list_steps_by_chain(&mut **self, chain_id).await
    }

    async fn next_step_in_chain(
        &mut self,
        chain_id: i64,
        current_sort_order: i32,
    ) -> Result<Option<TProcessChainStep>, sqlx::Error> {
        ProcessChainRepo::next_step_in_chain(&mut **self, chain_id, current_sort_order).await
    }

    async fn resolve_step_id_by_process(
        &mut self,
        chain_id: i64,
        process_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        ProcessChainRepo::resolve_step_id_by_process(&mut **self, chain_id, process_id).await
    }

    // ── mutate.rs bulk_insert_steps（1）──
    async fn bulk_insert_steps<'b>(
        &mut self,
        chain_id: i64,
        rows: &'b [NewProcessChainStep],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error> {
        ProcessChainRepo::bulk_insert_steps(&mut **self, chain_id, rows, snowflake, created_by)
            .await
    }

    // ── 跨域 helper（1）── 委托到 part 域 PartRepo 静态方法 ──────────
    async fn part_get_by_id(
        &mut self,
        part_id: i64,
        include_deleted: bool,
    ) -> Result<Option<crate::modules::part::model::TPart>, sqlx::Error> {
        use crate::modules::part::repo::PartRepo;
        PartRepo::get_by_id(&mut **self, part_id, include_deleted).await
    }
}