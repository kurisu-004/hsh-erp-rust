//! shelf 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 重构）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `ShelfRepoTrait`（10 方法，全部是
//!   `t_shelf` 自身操作），并直接 `impl ShelfRepoTrait for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 方法数演进
//! - 2026-09-22 初版：17 方法（t_shelf 12 + t_shelf_process 4 + 跨域 helper 2 的
//!   混合体，其中 t_shelf 侧实为 8 个对外方法 + 1 个账号计数 = 9；
//!   本文件 doc 当时误写「14 方法」，已一并订正）
//! - 2026-10-02 域拆分：**17 → 10 方法**（全部 `t_shelf`）
//!   - 删 4 个 `proc_*`（`t_shelf_process` 搬到 `prod::shelf_process::repo`）
//!   - 删 2 个跨域 helper（`proc_check_process_exists` /
//!     `proc_list_existing_process_ids`）—— 调用方一并删除，shelf → prod 反向
//!     依赖清零
//!   - 删 `count_accounts_by_shelf`（`ShelfOut.account_count` 出参取消，账号绑定
//!     真源在 iam 域）
//!
//! ## 为什么 trait 命名为 `ShelfRepoTrait` 而非 `ShelfRepo`
//! iam 范本里 trait 名 = `IamRepoTrait`。但 shelf 域有跨模块静态调用方（part 域
//! `worker_scan` / `phase1` / `inspection` 三个 service 文件都
//! `use crate::modules::shelf::repo::ShelfRepo;` 然后 `ShelfRepo::xxx(&mut *conn, ...)`
//! 走 ZST 静态方法）。**该 3 文件本次不在本任务范围**（属于 Group B/C/D/E），
//! 故本任务不能破坏 `shelf::repo::ShelfRepo` 作为 ZST 的对外身份。
//!
//! 解法：
//! - `shelf::repo::ShelfRepo` —— ZST struct（在 `sql.rs` 内，通过 `pub use sql::ShelfRepo;`
//!   重新导出至本模块），保留 t_shelf 静态方法签名不变（cross-module 调用方零修改）。
//! - `shelf::repo::ShelfRepoTrait` —— 本文件里的胖 trait，shelf 域内部 service 用
//!   `<R: ShelfRepoTrait>` 收。2026-10-02 起 trait 只剩 `t_shelf` 10 方法。
//!
//! ## 跨域依赖现状（2026-10-02：shelf → prod 依赖清零）
//! `t_shelf_process` 属 `prod::shelf_process`，`t_process` 属 `prod::process`，
//! 两者都移出本 trait。shelf 域现在**不**依赖任何 prod 模块；反向（prod 域读
//! `ShelfRepo::get_by_id` 校验货架存在 / scope）见
//! `src/modules/prod/shelf_process/service.rs`。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::ShelfRepo::yyy`。
//! 无需任何 `PgShelfRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockShelfRepoTrait` 供
//! service 单测注入。shelf 域当前无内联 mod tests（service 全部走
//! `tests/shelf/{api,deactivate}.rs` 集成测试守护），故未建 `shelf/service_tests/`
//! 目录——按 conventions.md §4.1 含 IO 不强求 100%。
//! 2026-10-02 核查：全仓 `MockShelfRepoTrait` 引用为**零**（只有本行 doc 提及），
//! 故本 trait 17 → 10 的收缩不影响任何单测。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，`map_duplicate_username` 检
//! SQLSTATE 23505 → 20502 BIZ_SHELF_DUPLICATE_CODE）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::modules::shelf::model::TShelf;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct / 表行类型，让上层继续用
// `super::repo::{TShelf, TShelfWithLoad, ShelfRepo}` 这种路径不破
// （cross-module 调用方都依赖这条路径）。
pub use sql::{ShelfRepo, TShelfWithLoad};

/// shelf 域数据访问 trait（10 方法，全部 `t_shelf`）。
///
/// 单 trait 而非每实体一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 重构定案；与 iam 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。
/// `<'a>` 显式生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ShelfRepoTrait: Send {
    // ── t_shelf（10）──
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
    async fn list_active_production_ordered(&mut self) -> Result<Vec<TShelfWithLoad>, sqlx::Error>;
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
}

/// 把 `ShelfRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借 `&mut *tx` 或
/// `&mut *conn` 即可调用 `sql::ShelfRepo::yyy`，零转发壳（与 iam 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::ShelfRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl ShelfRepoTrait for &mut PgConnection {
    // ── t_shelf（10）── 一行委托 sql::ShelfRepo ────────────────────
    async fn get_active_by_id(&mut self, id: i64) -> Result<Option<TShelf>, sqlx::Error> {
        ShelfRepo::get_active_by_id(&mut **self, id).await
    }

    async fn get_by_id(&mut self, id: i64) -> Result<Option<TShelf>, sqlx::Error> {
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
        ShelfRepo::list_with_filters(&mut **self, code_like, zone, is_active, limit, offset).await
    }

    async fn count_with_filters<'b>(
        &mut self,
        code_like: Option<&'b str>,
        zone: Option<&'b str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        ShelfRepo::count_with_filters(&mut **self, code_like, zone, is_active).await
    }

    async fn list_active_production_ordered(&mut self) -> Result<Vec<TShelfWithLoad>, sqlx::Error> {
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

    async fn count_in_use_parts(&mut self, shelf_id: i64) -> Result<i64, sqlx::Error> {
        ShelfRepo::count_in_use_parts(&mut **self, shelf_id).await
    }
}
