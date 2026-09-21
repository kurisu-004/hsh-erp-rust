//! shelf 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 重构）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，8 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `ShelfRepoTrait`（12 方法合并单 trait；
//!   含 t_shelf 8 方法 + t_shelf_process 4 方法 + 2 个跨域 helper），并直接
//!   `impl ShelfRepoTrait for &mut PgConnection`——handler/service 借 `&mut *tx` /
//!   `&mut *conn` 即可，零中间壳。
//! - `crate::modules::shelf::process_mapping::sql::ShelfProcessRepo`：t_shelf_process
//!   的 SQL 真源（4 静态方法）；胖 trait 把这 4 个方法也合并进来，由 trait impl
//!   一行委托到此 ZST。
//!
//! ## 为什么 trait 命名为 `ShelfRepoTrait` 而非 `ShelfRepo`
//! iam 范本里 trait 名 = `IamRepo`。但 shelf 域有跨模块静态调用方（part 域
//! `worker_scan` / `phase1` / `inspection` 三个 service 文件都
//! `use crate::modules::shelf::repo::ShelfRepo;` 然后 `ShelfRepo::xxx(&mut *conn, ...)`
//! 走 ZST 静态方法）。**该 3 文件本次不在本任务范围**（属于 Group B/C/D/E），
//! 故本任务不能破坏 `shelf::repo::ShelfRepo` 作为 ZST 的对外身份。
//!
//! 解法：
//! - `shelf::repo::ShelfRepo` —— ZST struct（在 `sql.rs` 内，通过 `pub use sql::ShelfRepo;`
//!   重新导出至本模块），保留 8 个 t_shelf 静态方法签名不变（cross-module 调用方零修改）。
//! - `shelf::repo::ShelfRepoTrait` —— 本文件新加的胖 trait，shelf 域内部 service 用
//!   `<R: ShelfRepoTrait>` 收。trait 方法数 14（t_shelf 8 + t_shelf_process 4 + 跨域
//!   helper 2）。
//!
//! ## 为什么是胖 trait 合并 t_shelf_process
//! - 决策方案 A（推荐）：trait 见 `ShelfRepoTrait` 含 t_shelf + t_shelf_process 全部 12 方法。
//! - 理由：service 常在同一调用链中交替访问两类表（特别是 `set_shelf_processes` 既要
//!   `get_by_id` 也要 `proc_soft_delete_all_for_shelf` + `proc_bulk_insert`）。胖 trait
//!   让 service 签名统一为 `<R: ShelfRepoTrait>(&self, mut repo: R, ...)`，单借位、
//!   单 mock、单 cargo clippy 校验面。
//!
//! ## 跨域 helper（`proc_check_process_exists` / `proc_list_existing_ids`）
//! shelf `list_for_return` / `set_shelf_processes` 需要查 `t_process`（prod 域）。
//! prod 域暂未对外暴露 trait（属于 Group B/C/D/E），无法让 shelf service 收
//! `<R: ProcRepo>`。替代方案：把跨域调用封装到 shelf trait 的 helper 方法中——
//! trait impl 在 `&mut PgConnection` 上时一行委托给 `ProcessRepo::xxx` 静态方法。
//! 这样 service 仍只需一个 `repo: R: ShelfRepoTrait` 参数。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::ShelfRepo::yyy`。
//! 无需任何 `PgShelfRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockShelfRepoTrait` 供
//! service 单测注入。shelf 域当前无内联 mod tests（service 全部走
//! `tests/shelf_api.rs` + `tests/worker_shelf_deactivate_api.rs` 集成测试守护），
//! 故未建 `shelf/service_tests/` 目录——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，`map_duplicate_username` 检
//! SQLSTATE 23505 → 20502 BIZ_SHELF_DUPLICATE_CODE）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::shelf::model::TShelf;
use crate::modules::shelf::process_mapping::sql::{NewShelfProcessRow, ShelfProcessRepo};

pub mod sql;

// 重导出 sql.rs 中的 ZST struct / 表行类型，让上层继续用
// `super::repo::{TShelf, TShelfWithLoad, ShelfRepo, NewShelfProcessRow, ShelfProcessRepo}`
// 这种路径不破（cross-module 调用方都依赖这条路径）。
pub use sql::{ShelfRepo, TShelfWithLoad};

/// shelf 域数据访问 trait（14 方法 = t_shelf 8 + t_shelf_process 4 + 跨域 helper 2）。
///
/// 单 trait 而非每实体一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 重构定案；与 iam 同形）。
///
/// 方法签名 = `sql.rs` / `process_mapping::sql.rs` 固有静态方法去 executor 形参 +
/// 跨域 helper（trait impl 一行委托到 prod 域的 `ProcessRepo` 静态方法）。
/// `<'a>` 显式生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ShelfRepoTrait: Send {
    // ── t_shelf（8）──
    async fn get_active_by_id(&mut self, id: i64) -> Result<Option<TShelf>, sqlx::Error>;
    async fn get_by_id(&mut self, id: i64) -> Result<Option<TShelf>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn get_by_id_zone<'a>(
        &mut self,
        id: i64,
        zone: &'a str,
    ) -> Result<Option<TShelf>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn list_with_filters<'a>(
        &mut self,
        code_like: Option<&'a str>,
        zone: Option<&'a str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TShelf>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &mut self,
        code_like: Option<&'a str>,
        zone: Option<&'a str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error>;
    async fn list_active_production_ordered(&mut self)
        -> Result<Vec<TShelfWithLoad>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn create<'a>(
        &mut self,
        snowflake_id: i64,
        code: &'a str,
        name: &'a str,
        zone: &'a str,
        location: Option<&'a str>,
        display_order: i32,
        created_by: i64,
    ) -> Result<TShelf, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update<'a>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'a str>,
        location: Option<Option<&'a str>>,
        display_order: Option<i32>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn count_in_use_parts(&mut self, shelf_id: i64) -> Result<i64, sqlx::Error>;
    async fn count_accounts_by_shelf(
        &mut self,
        shelf_ids: &[i64],
    ) -> Result<Vec<(i64, i64)>, sqlx::Error>;

    // ── t_shelf_process（4）── 用 proc_ 前缀消歧义
    #[allow(clippy::type_complexity)]
    async fn proc_list_by_shelf(
        &mut self,
        shelf_id: i64,
    ) -> Result<Vec<(i64, i64, i32, String, String)>, sqlx::Error>;
    async fn proc_list_all_active_mappings(
        &mut self,
    ) -> Result<Vec<(i64, i64, String, String)>, sqlx::Error>;
    async fn proc_soft_delete_all_for_shelf(
        &mut self,
        shelf_id: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn proc_bulk_insert(
        &mut self,
        rows: &[NewShelfProcessRow],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── 跨域 helper（2）── 委托到 prod 域 ProcessRepo 静态方法，避免 service 收第二个 conn
    /// 检查 `process_id` 是否存在（`ProcessRepo::get_by_id(pid, false).await?.is_some()`）。
    /// 用于 `list_for_return` 校验 `next_process_id`。
    async fn proc_check_process_exists(&mut self, process_id: i64) -> Result<bool, sqlx::Error>;
    /// 批量查 process：`ProcessRepo::list_by_ids(ids).await?`。
    /// 用于 `set_shelf_processes` 校验 items 内的全部 process_id 存在。
    async fn proc_list_existing_process_ids<'a>(
        &mut self,
        process_ids: &'a [i64],
    ) -> Result<std::collections::HashSet<i64>, sqlx::Error>;
}

/// 把 `ShelfRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借 `&mut *tx` 或
/// `&mut *conn` 即可调用 `sql::XxxRepo::yyy`，零转发壳（与 iam 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::XxxRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl ShelfRepoTrait for &mut PgConnection {
    // ── t_shelf（8）── 一行委托 sql::ShelfRepo ────────────────────
    async fn get_active_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        ShelfRepo::get_active_by_id(&mut **self, id).await
    }

    async fn get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        ShelfRepo::get_by_id(&mut **self, id).await
    }

    async fn get_by_id_zone<'b>(
        &mut self,
        id: i64,
        zone: &'b str,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        ShelfRepo::get_by_id_zone(&mut **self, id, zone).await
    }

    async fn list_with_filters<'b>(
        &mut self,
        code_like: Option<&'b str>,
        zone: Option<&'b str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TShelf>, sqlx::Error> {
        ShelfRepo::list_with_filters(&mut **self, code_like, zone, is_active, limit, offset)
            .await
    }

    async fn count_with_filters<'b>(
        &mut self,
        code_like: Option<&'b str>,
        zone: Option<&'b str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        ShelfRepo::count_with_filters(&mut **self, code_like, zone, is_active).await
    }

    async fn list_active_production_ordered(
        &mut self,
    ) -> Result<Vec<TShelfWithLoad>, sqlx::Error> {
        ShelfRepo::list_active_production_ordered(&mut **self).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create<'b>(
        &mut self,
        snowflake_id: i64,
        code: &'b str,
        name: &'b str,
        zone: &'b str,
        location: Option<&'b str>,
        display_order: i32,
        created_by: i64,
    ) -> Result<TShelf, sqlx::Error> {
        ShelfRepo::create(
            &mut **self,
            snowflake_id,
            code,
            name,
            zone,
            location,
            display_order,
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
        location: Option<Option<&'b str>>,
        display_order: Option<i32>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        ShelfRepo::update(
            &mut **self,
            id,
            version,
            name,
            location,
            display_order,
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
        ShelfRepo::soft_delete(&mut **self, id, version, updated_by).await
    }

    async fn count_in_use_parts(
        &mut self,
        shelf_id: i64,
    ) -> Result<i64, sqlx::Error> {
        ShelfRepo::count_in_use_parts(&mut **self, shelf_id).await
    }

    async fn count_accounts_by_shelf(
        &mut self,
        shelf_ids: &[i64],
    ) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        ShelfRepo::count_accounts_by_shelf(&mut **self, shelf_ids).await
    }

    // ── t_shelf_process（4）── 一行委托 process_mapping::sql::ShelfProcessRepo ─
    async fn proc_list_by_shelf(
        &mut self,
        shelf_id: i64,
    ) -> Result<Vec<(i64, i64, i32, String, String)>, sqlx::Error> {
        ShelfProcessRepo::list_by_shelf(&mut **self, shelf_id).await
    }

    async fn proc_list_all_active_mappings(
        &mut self,
    ) -> Result<Vec<(i64, i64, String, String)>, sqlx::Error> {
        ShelfProcessRepo::list_all_active_mappings(&mut **self).await
    }

    async fn proc_soft_delete_all_for_shelf(
        &mut self,
        shelf_id: i64,
    ) -> Result<u64, sqlx::Error> {
        ShelfProcessRepo::soft_delete_all_for_shelf(&mut **self, shelf_id).await
    }

    async fn proc_bulk_insert(
        &mut self,
        rows: &[NewShelfProcessRow],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error> {
        ShelfProcessRepo::bulk_insert(&mut **self, rows, snowflake, created_by).await
    }

    // ── 跨域 helper（2）── 委托 prod 域 ProcessRepo 静态方法 ──────────
    async fn proc_check_process_exists(
        &mut self,
        process_id: i64,
    ) -> Result<bool, sqlx::Error> {
        use crate::modules::prod::process::repo::ProcessRepo;
        Ok(ProcessRepo::get_by_id(&mut **self, process_id, false)
            .await?
            .is_some())
    }

    async fn proc_list_existing_process_ids<'b>(
        &mut self,
        process_ids: &'b [i64],
    ) -> Result<std::collections::HashSet<i64>, sqlx::Error> {
        use crate::modules::prod::process::repo::ProcessRepo;
        let existing = ProcessRepo::list_by_ids(&mut **self, process_ids).await?;
        Ok(existing.iter().map(|p| p.id).collect())
    }
}