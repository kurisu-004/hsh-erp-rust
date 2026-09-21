//! applicant 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 重构对齐 iam 范本）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，9 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `ApplicantRepo`（9 方法合并单 trait；
//!   t_applicant 7 + t_customer 跨域 helper 1 `customer_name` + 跨域校验 1
//!   `l1_customer_exists`），并直接 `impl ApplicantRepo for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `ApplicantRepoTrait`（带 `Trait` 后缀）
//! 与 shelf / customer 同形：跨模块静态调用方（本任务范围内未发现，保留扩展点）
//! 会直接 `ApplicantRepo::xxx(&mut *conn, ...)` 走 ZST 静态方法。`com::customer::repo::CustomerRepo`
//! 已被 part/delivery_note/_e2e 静态调用（10+ 处），为对称与未来一致性，
//! 申请人 trait 也加 `Trait` 后缀，避免后续 part 域引入 applicant 静态调用时再破。
//!
//! - `com::applicant::repo::ApplicantRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::ApplicantRepo;` 重新导出至本模块），保留 9 个 pub 静态方法签名不变。
//! - `com::applicant::repo::ApplicantRepoTrait` —— 本文件新加的胖 trait（9 方法），
//!   applicant 域内部 service 用 `<R: ApplicantRepoTrait>` 收。
//!
//! ## 跨域 trait 形参（`CustomerRepo`）
//! applicant service `list_applicants` 需批量补 `customer_name`，原 `service.rs`
//! inline SQL `SELECT id, name FROM t_customer WHERE id = ANY($1) ...` 收敛到
//! `CustomerRepo::lookup_names`。两个 trait（`ApplicantRepo` + `CustomerRepo`）独立，
//! handler 借 `&mut *tx` 喂两次 reborrow（见 applicant/handler.rs），service 形参
//! `<R: ApplicantRepoTrait, R3: CustomerRepoTrait>` by-value 各自独立单借位。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都
//! `DerefMut<Target = PgConnection>`，故 `&mut *tx` / `&mut *conn` 即
//! `&mut PgConnection`，可直接喂给 `sql::ApplicantRepo::yyy`。
//! 无需任何 `PgApplicantRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockApplicantRepo`
//! 供 service 单测注入。applicant 域当前无内联 mod tests（service 全部走
//! `tests/applicant_api.rs` 集成测试守护），故未建 `applicant/service_tests/` 目录
//! ——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，`create` 检
//! SQLSTATE 23505 → 21002 BIZ_APPLICANT_DUPLICATE_NAME 由 service 层做）。

use async_trait::async_trait;
use sqlx::PgConnection;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{TApplicant, ApplicantRepo}` 这种路径不破。
pub use super::model::TApplicant;
pub use sql::ApplicantRepo;

/// applicant 域数据访问 trait（9 方法 = t_applicant 7 + t_customer 跨域 2）。
///
/// 单 trait 而非每表一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 重构定案；与 iam 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。`<'a>` 显式生命周期是
/// mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ApplicantRepoTrait: Send {
    // ── t_applicant（7）──
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TApplicant>, sqlx::Error>;
    async fn find_by_name_and_customer<'a>(
        &mut self,
        name: &'a str,
        customer_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TApplicant>, sqlx::Error>;
    async fn list_with_filters<'a>(
        &mut self,
        customer_id: Option<i64>,
        name_like: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TApplicant>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &mut self,
        customer_id: Option<i64>,
        name_like: Option<&'a str>,
    ) -> Result<i64, sqlx::Error>;
    async fn create<'a>(
        &mut self,
        snowflake_id: i64,
        name: &'a str,
        customer_id: i64,
        created_by: Option<i64>,
    ) -> Result<(), sqlx::Error>;
    async fn update<'a>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'a str>,
        customer_id: Option<i64>,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── t_customer 跨域 helper（2）──
    /// 单值读 `t_customer.name`（仅未软删）。用于 applicant `get` / `create` /
    /// `update` 后回填单条 `customer_name`（区别于批量 `CustomerRepo::lookup_names`）。
    async fn customer_name(
        &mut self,
        customer_id: i64,
    ) -> Result<Option<String>, sqlx::Error>;
    /// 校验 `customer_id` 是否指向 L1 客户（`parent_id IS NULL`）。`true` ⇒ 是 L1。
    async fn l1_customer_exists(
        &mut self,
        customer_id: i64,
    ) -> Result<bool, sqlx::Error>;

    // ── t_part 跨域校验（1）──
    /// 软删前「被 part 引用」校验：返回未软删 part 数。
    async fn count_parts_using_applicant_name<'a>(
        &mut self,
        name: &'a str,
        customer_id: i64,
    ) -> Result<i64, sqlx::Error>;
}

/// 把 `ApplicantRepo` 直接对 `&mut PgConnection` 实现——handler/service 借 `&mut *tx` 或
/// `&mut *conn` 即可调用 `sql::ApplicantRepo::yyy`，零转发壳（与 iam 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::ApplicantRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl ApplicantRepoTrait for &mut PgConnection {
    // ── t_applicant（7）── 一行委托 sql::ApplicantRepo ─────────────
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TApplicant>, sqlx::Error> {
        ApplicantRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn find_by_name_and_customer<'b>(
        &mut self,
        name: &'b str,
        customer_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TApplicant>, sqlx::Error> {
        ApplicantRepo::find_by_name_and_customer(&mut **self, name, customer_id, include_deleted)
            .await
    }

    async fn list_with_filters<'b>(
        &mut self,
        customer_id: Option<i64>,
        name_like: Option<&'b str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TApplicant>, sqlx::Error> {
        ApplicantRepo::list_with_filters(&mut **self, customer_id, name_like, limit, offset).await
    }

    async fn count_with_filters<'b>(
        &mut self,
        customer_id: Option<i64>,
        name_like: Option<&'b str>,
    ) -> Result<i64, sqlx::Error> {
        ApplicantRepo::count_with_filters(&mut **self, customer_id, name_like).await
    }

    async fn create<'b>(
        &mut self,
        snowflake_id: i64,
        name: &'b str,
        customer_id: i64,
        created_by: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        ApplicantRepo::create(&mut **self, snowflake_id, name, customer_id, created_by).await
    }

    async fn update<'b>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'b str>,
        customer_id: Option<i64>,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        ApplicantRepo::update(&mut **self, id, version, name, customer_id, updated_by).await
    }

    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        ApplicantRepo::soft_delete(&mut **self, id, version, updated_by).await
    }

    // ── t_customer 跨域 helper（2）── 一行委托 sql::ApplicantRepo ──
    async fn customer_name(
        &mut self,
        customer_id: i64,
    ) -> Result<Option<String>, sqlx::Error> {
        ApplicantRepo::customer_name(&mut **self, customer_id).await
    }

    async fn l1_customer_exists(
        &mut self,
        customer_id: i64,
    ) -> Result<bool, sqlx::Error> {
        ApplicantRepo::l1_customer_exists(&mut **self, customer_id).await
    }

    // ── t_part 跨域校验（1）──
    async fn count_parts_using_applicant_name<'b>(
        &mut self,
        name: &'b str,
        customer_id: i64,
    ) -> Result<i64, sqlx::Error> {
        ApplicantRepo::count_parts_using_applicant_name(&mut **self, name, customer_id).await
    }
}
