//! customer 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 重构对齐 iam 范本）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，10 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变；新加的 `lookup_names` /
//!   `count_parts_using_customer` / `count_assemblies_using_customer` 三个方法
//!   承载原本散落在 `service.rs` 的 inline 跨域 SQL，SQL 字符串完全不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `CustomerRepo`（11 方法合并单 trait；
//!   t_customer 8 + 跨域 helper 3），并直接 `impl CustomerRepo for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么是胖 trait 而不是按表拆 2 trait
//! 与 iam 范本同形（见 `iam/repo/mod.rs` §「为什么是胖 trait」）：
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例；service 同时
//! 持有 `customer_repo` + 跨域 helper 时只能走单借位。trait `CustomerRepo`
//! 是单借位，service 签名 `<R: CustomerRepoTrait>(&self, mut repo: R, ...)` 一次收下。
//!
//! ## 为什么 trait 命名为 `CustomerRepoTrait`（带 `Trait` 后缀）
//! 与 shelf 同形（`ShelfRepoTrait`）：跨模块静态调用方 `part` / `delivery_note` /
//! `_e2e` 共 10+ 处直接 `CustomerRepo::get_by_id(&mut *conn, ...)` 走 ZST 静态方法，
//! 本任务**不能**破坏 `com::customer::repo::CustomerRepo` 作为 ZST 的对外身份，
//! 故 trait 必须改名 `CustomerRepoTrait`。
//!
//! - `com::customer::repo::CustomerRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::CustomerRepo;` 重新导出至本模块），保留 10 个 pub 静态方法签名
//!   不变（cross-module 调用方零修改；2026-09-22 重构唯一例外：`lookup_names` /
//!   `count_parts_using_customer` / `count_assemblies_using_customer` 是新增 helper，
//!   无外部调用方）。
//! - `com::customer::repo::CustomerRepoTrait` —— 本文件新加的胖 trait（11 方法），
//!   customer 域内部 service 用 `<R: CustomerRepoTrait>` 收。
//!
//! ## 跨域 helper（`lookup_names` / `count_parts_using_customer` /
//! ## `count_assemblies_using_customer`）
//! 原本散落在 `applicant/service.rs` 与 `customer/service.rs` 的 inline SQL，
//! 抽 trait 时统一收敛到 `CustomerRepo`：service 拿 trait 即可，零 inline SQL。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都
//! `DerefMut<Target = PgConnection>`，故 `&mut *tx` / `&mut *conn` 即
//! `&mut PgConnection`，可直接喂给 `sql::CustomerRepo::yyy`。
//! 无需任何 `PgCustomerRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockCustomerRepo`
//! 供 service 单测注入。customer 域当前无内联 mod tests（service 全部走
//! `tests/customer_api.rs` 集成测试守护），故未建 `customer/service_tests/` 目录
//! ——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，`create` 检
//! SQLSTATE 23505 → 20104 BIZ_INVALID_VALUE 已在 service 层做）。

use async_trait::async_trait;
use sqlx::PgConnection;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{TCustomer, CustomerRepo}` 这种路径不破。
pub use super::model::TCustomer;
pub use sql::CustomerRepo;

/// customer 域数据访问 trait（11 方法 = t_customer 8 + 跨域 helper 3）。
///
/// 单 trait 而非每表一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 重构定案；与 iam 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。`<'a>` 显式生命周期是
/// mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait CustomerRepoTrait: Send {
    // ── t_customer（8）──
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TCustomer>, sqlx::Error>;
    async fn list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error>;
    async fn list_children(
        &mut self,
        parent_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error>;
    async fn list_roots(
        &mut self,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error>;
    async fn list_all(
        &mut self,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn list_with_filters<'a>(
        &mut self,
        name_like: Option<&'a str>,
        parent_id: Option<i64>,
        is_root: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TCustomer>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &mut self,
        name_like: Option<&'a str>,
        parent_id: Option<i64>,
        is_root: Option<bool>,
    ) -> Result<i64, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn create<'a>(
        &mut self,
        snowflake_id: i64,
        name: &'a str,
        parent_id: Option<i64>,
        serial_prefix: Option<&'a str>,
        created_by: i64,
    ) -> Result<TCustomer, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update<'a>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'a str>,
        serial_prefix: Option<Option<&'a str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── 跨域 helper（3）──
    /// 批量查 `(id, name)`（仅未软删）。供 `applicant` 域 `list_applicants` 批量补
    /// `customer_name` 用。空 `ids` 短路返回 `Vec::new()`。
    async fn lookup_names<'a>(
        &mut self,
        ids: &'a [i64],
    ) -> Result<Vec<(i64, String)>, sqlx::Error>;
    /// `t_part` 引用本 customer 的非软删计数。供本域 `soft_delete_customer` 校验用。
    async fn count_parts_using_customer(
        &mut self,
        customer_id: i64,
    ) -> Result<i64, sqlx::Error>;
    /// `t_assembly` 引用本 customer 的非软删计数。供本域 `soft_delete_customer` 校验用。
    async fn count_assemblies_using_customer(
        &mut self,
        customer_id: i64,
    ) -> Result<i64, sqlx::Error>;
}

/// 把 `CustomerRepo` 直接对 `&mut PgConnection` 实现——handler/service 借 `&mut *tx` 或
/// `&mut *conn` 即可调用 `sql::CustomerRepo::yyy`，零转发壳（与 iam 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::CustomerRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl CustomerRepoTrait for &mut PgConnection {
    // ── t_customer（8）── 一行委托 sql::CustomerRepo ──────────────
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TCustomer>, sqlx::Error> {
        CustomerRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn list_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        CustomerRepo::list_by_ids(&mut **self, ids, include_deleted).await
    }

    async fn list_children(
        &mut self,
        parent_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        CustomerRepo::list_children(&mut **self, parent_id, include_deleted).await
    }

    async fn list_roots(
        &mut self,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        CustomerRepo::list_roots(&mut **self, include_deleted).await
    }

    async fn list_all(
        &mut self,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        CustomerRepo::list_all(&mut **self, include_deleted).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_with_filters<'b>(
        &mut self,
        name_like: Option<&'b str>,
        parent_id: Option<i64>,
        is_root: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        CustomerRepo::list_with_filters(
            &mut **self,
            name_like,
            parent_id,
            is_root,
            limit,
            offset,
        )
        .await
    }

    async fn count_with_filters<'b>(
        &mut self,
        name_like: Option<&'b str>,
        parent_id: Option<i64>,
        is_root: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        CustomerRepo::count_with_filters(&mut **self, name_like, parent_id, is_root).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create<'b>(
        &mut self,
        snowflake_id: i64,
        name: &'b str,
        parent_id: Option<i64>,
        serial_prefix: Option<&'b str>,
        created_by: i64,
    ) -> Result<TCustomer, sqlx::Error> {
        CustomerRepo::create(
            &mut **self,
            snowflake_id,
            name,
            parent_id,
            serial_prefix,
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
        serial_prefix: Option<Option<&'b str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        CustomerRepo::update(&mut **self, id, version, name, serial_prefix, updated_by).await
    }

    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        CustomerRepo::soft_delete(&mut **self, id, version, updated_by).await
    }

    // ── 跨域 helper（3）── 一行委托 sql::CustomerRepo ─────────────
    async fn lookup_names<'b>(
        &mut self,
        ids: &'b [i64],
    ) -> Result<Vec<(i64, String)>, sqlx::Error> {
        CustomerRepo::lookup_names(&mut **self, ids).await
    }

    async fn count_parts_using_customer(
        &mut self,
        customer_id: i64,
    ) -> Result<i64, sqlx::Error> {
        CustomerRepo::count_parts_using_customer(&mut **self, customer_id).await
    }

    async fn count_assemblies_using_customer(
        &mut self,
        customer_id: i64,
    ) -> Result<i64, sqlx::Error> {
        CustomerRepo::count_assemblies_using_customer(&mut **self, customer_id).await
    }
}
