//! iam 域 repo 层（SQL 真源 + trait + PG 实现）
//!
//! ## 结构（2026-09-22 重构：删 `PgIamRepo` 转发壳）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，17 个 pub 固有静态方法 + sqlx `query!` 宏，**内容零 diff**
//!   （`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `IamRepo`（17 方法合并单 trait；2026-09-21 定案），
//!   re-export `sql.rs` 的 model / struct / row；并直接 `impl IamRepo for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么是胖 trait 而不是按实体拆 4 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例；如果按实体拆 4 trait，service
//! 同时使用 user_repo + user_role_repo 时无法表达「同连接两次借用」。胖 trait `IamRepo`
//! 是单借位，service 签名 `<R: IamRepo>(&self, repo: &mut R, ...)` 一次借出，方法体内
//! 全部 `repo.xxx()` 都走同一连接。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::XxxRepo::yyy`。
//! 旧 `PgIamRepo<'a>` 转发壳只重复了一次 reborrow，没有任何额外价值，2026-09-22 删除。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockIamRepo` 供
//! `service_tests` 注入。方法签名里的 `<'a>` 显式生命周期是 mockall 0.15 + async_trait
//! 的硬性要求（沿用原 uow.rs 注释结论）。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，`map_duplicate_username` 检
//! SQLSTATE 23505）。

use async_trait::async_trait;
use chrono::NaiveDateTime;
use sqlx::PgConnection;

pub mod sql;

// 重导出 sql.rs 中的 struct / insert / row 与 model 的表行类型，让上层继续用
// `super::repo::{User, UserInsert, ...}` 这种路径不破。
pub use crate::modules::iam::model::{Menu, Shelf, User, UserRole};
pub use sql::{MenuRepo, ShelfRepo, UserRepo, UserRoleRepo, UserInsert, UserRoleInsert, UserRoleRow};

/// iam 域数据访问 trait（17 方法 = t_user 10 + t_user_role 5 + t_menu 1 + t_shelf 1）。
///
/// 单 trait 而非每实体一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-21 重构定案）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。`<'a>` 显式生命周期是 mockall
/// 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait IamRepo: Send {
    // ── t_user（10）──
    async fn get_by_id(&mut self, id: i64) -> Result<Option<User>, sqlx::Error>;
    async fn get_by_username<'a>(
        &mut self,
        username_lower: &'a str,
    ) -> Result<Option<User>, sqlx::Error>;
    async fn list_with_filters<'a>(
        &mut self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<User>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &mut self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error>;
    async fn create(&mut self, user: &UserInsert) -> Result<(), sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update_partial<'a>(
        &mut self,
        id: i64,
        version: i32,
        full_name: Option<&'a str>,
        set_phone: bool,
        phone: Option<&'a str>,
        password_hash: Option<&'a str>,
        is_active: Option<bool>,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn touch_login(&mut self, id: i64, when: NaiveDateTime) -> Result<(), sqlx::Error>;
    async fn increment_refresh_token_version(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn update_password_and_rotate<'a>(
        &mut self,
        id: i64,
        version: i32,
        password_hash: &'a str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── t_user_role（5）── 用 role_ 前缀消歧义
    async fn list_by_user(&mut self, user_id: i64) -> Result<Vec<UserRoleRow>, sqlx::Error>;
    async fn role_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<UserRole>, sqlx::Error>;
    async fn exists_same_scope<'a>(
        &mut self,
        user_id: i64,
        role: &'a str,
        scope_type: Option<&'a str>,
        scope_id: Option<i64>,
    ) -> Result<bool, sqlx::Error>;
    async fn role_create(&mut self, role_row: &UserRoleInsert) -> Result<(), sqlx::Error>;
    async fn role_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── t_menu（1）──
    async fn list_active_for_roles<'a>(
        &mut self,
        roles: &'a [String],
    ) -> Result<Vec<Menu>, sqlx::Error>;

    // ── t_shelf（1）──
    async fn shelf_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<Shelf>, sqlx::Error>;
}

/// 把 `IamRepo` 直接对 `&mut PgConnection` 实现——handler/service 借 `&mut *tx` 或
/// `&mut *conn` 即可调用 `sql::XxxRepo::yyy`，零转发壳（2026-09-22 替代 `PgIamRepo<'a>`）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::XxxRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl IamRepo for &mut PgConnection {
    // ── t_user（10）── 一行委托 sql::UserRepo ────────────────────
    async fn get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<User>, sqlx::Error> {
        UserRepo::get_by_id(&mut **self, id).await
    }

    async fn get_by_username<'b>(
        &mut self,
        username_lower: &'b str,
    ) -> Result<Option<User>, sqlx::Error> {
        UserRepo::get_by_username(&mut **self, username_lower).await
    }

    async fn list_with_filters<'b>(
        &mut self,
        username_like: Option<&'b str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<User>, sqlx::Error> {
        UserRepo::list_with_filters(&mut **self, username_like, is_active, limit, offset).await
    }

    async fn count_with_filters<'b>(
        &mut self,
        username_like: Option<&'b str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        UserRepo::count_with_filters(&mut **self, username_like, is_active).await
    }

    async fn create(
        &mut self,
        user: &UserInsert,
    ) -> Result<(), sqlx::Error> {
        UserRepo::create(&mut **self, user).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_partial<'b>(
        &mut self,
        id: i64,
        version: i32,
        full_name: Option<&'b str>,
        set_phone: bool,
        phone: Option<&'b str>,
        password_hash: Option<&'b str>,
        is_active: Option<bool>,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRepo::update_partial(
            &mut **self,
            id,
            version,
            full_name,
            set_phone,
            phone,
            password_hash,
            is_active,
            when,
            updated_by,
        )
        .await
    }

    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRepo::soft_delete(&mut **self, id, version, when, updated_by).await
    }

    async fn touch_login(&mut self, id: i64, when: NaiveDateTime) -> Result<(), sqlx::Error> {
        UserRepo::touch_login(&mut **self, id, when).await
    }

    async fn increment_refresh_token_version(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRepo::increment_refresh_token_version(&mut **self, id, version, when, updated_by).await
    }

    async fn update_password_and_rotate<'b>(
        &mut self,
        id: i64,
        version: i32,
        password_hash: &'b str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRepo::update_password_and_rotate(
            &mut **self,
            id,
            version,
            password_hash,
            when,
            updated_by,
        )
        .await
    }

    // ── t_user_role（5）── 一行委托 sql::UserRoleRepo ────────────
    async fn list_by_user(
        &mut self,
        user_id: i64,
    ) -> Result<Vec<UserRoleRow>, sqlx::Error> {
        UserRoleRepo::list_by_user(&mut **self, user_id).await
    }

    async fn role_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<UserRole>, sqlx::Error> {
        UserRoleRepo::get_by_id(&mut **self, id).await
    }

    async fn exists_same_scope<'b>(
        &mut self,
        user_id: i64,
        role: &'b str,
        scope_type: Option<&'b str>,
        scope_id: Option<i64>,
    ) -> Result<bool, sqlx::Error> {
        UserRoleRepo::exists_same_scope(&mut **self, user_id, role, scope_type, scope_id).await
    }

    async fn role_create(
        &mut self,
        role_row: &UserRoleInsert,
    ) -> Result<(), sqlx::Error> {
        UserRoleRepo::create(&mut **self, role_row).await
    }

    async fn role_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRoleRepo::soft_delete(&mut **self, id, version, when, updated_by).await
    }

    // ── t_menu（1）──
    async fn list_active_for_roles<'b>(
        &mut self,
        roles: &'b [String],
    ) -> Result<Vec<Menu>, sqlx::Error> {
        MenuRepo::list_active_for_roles(&mut **self, roles).await
    }

    // ── t_shelf（1）──
    async fn shelf_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<Shelf>, sqlx::Error> {
        ShelfRepo::get_by_id(&mut **self, id).await
    }
}