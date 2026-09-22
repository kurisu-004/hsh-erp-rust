//! iam 域 SQL 真源（按实体拆 4 子文件）+ 单一 `impl IamRepo for &mut PgConnection` 块
//!
//! ## 结构（2026-09-22 重构 #2）
//! - `user.rs`        — `t_user` 10 个 SQL fns + `UserInsert` / `UserPartialUpdate<'_>` 入参
//! - `user_role.rs`   — `t_user_role` 5 个 SQL fns + `UserRoleInsert` / `UserRoleRow`
//! - `menu.rs`        — `t_menu` 1 个 SQL fn
//! - `shelf.rs`       — `t_shelf` 1 个 SQL fn（本域只读）
//! - `mod.rs`（本文件）— 声明子模块 + **单一** `impl IamRepo for &mut PgConnection` 块
//!   （覆盖全部 17 方法，按实体分组；call 各子文件 free fn）
//!
//! ## 为什么不是 4 个分散 impl 块
//! Rust coherence 规则：同 crate 内同一 trait 对同一类型至多一个 impl 块（auto trait
//! 例外：Send/Sync/Unpin）。本想每文件就地 impl，编译报 E0119 冲突——故统一收到本文件，
//! SQL 子文件仅放「真源」（free fn + 入参 DTO），调用点 `super::user::xxx`。
//!
//! ## SQL 真源 → trait 方法的薄委托
//! impl 块里每方法只一行 `super::xxx::yyy(&mut **self, ...).await`，无业务分支；
//! 全部 17 行的 `&mut **self` reborrow（避免 move 引用本身）见 `repo/mod.rs` 注释。

pub mod menu;
pub mod shelf;
pub mod user;
pub mod user_role;

use async_trait::async_trait;
use chrono::NaiveDateTime;
use sqlx::PgConnection;

use crate::modules::iam::repo::model::{Menu, Shelf, User, UserRole};
use crate::modules::iam::repo::{IamRepo, UserInsert, UserPartialUpdate, UserRoleInsert, UserRoleRow};

/// 统一 `IamRepo for &mut PgConnection` 实现（按实体分组，零业务逻辑）
///
/// Rust 同一类型 + trait 至多一个 impl 块，故本块收在此处；子文件 sql/* 仅放 SQL 真源。
#[async_trait]
impl IamRepo for &mut PgConnection {
    // ── t_user（10）──
    async fn get_user_by_id(&mut self, id: i64) -> Result<Option<User>, sqlx::Error> {
        user::get_user_by_id(&mut **self, id).await
    }
    async fn get_user_by_username<'b>(
        &mut self,
        username_lower: &'b str,
    ) -> Result<Option<User>, sqlx::Error> {
        user::get_user_by_username(&mut **self, username_lower).await
    }
    async fn list_users_with_filters<'b>(
        &mut self,
        username_like: Option<&'b str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<User>, sqlx::Error> {
        user::list_users_with_filters(&mut **self, username_like, is_active, limit, offset).await
    }
    async fn count_users_with_filters<'b>(
        &mut self,
        username_like: Option<&'b str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        user::count_users_with_filters(&mut **self, username_like, is_active).await
    }
    async fn create_user(&mut self, user: &UserInsert) -> Result<(), sqlx::Error> {
        user::create_user(&mut **self, user).await
    }
    async fn update_user_partial<'b>(
        &mut self,
        id: i64,
        version: i32,
        args: &UserPartialUpdate<'b>,
    ) -> Result<u64, sqlx::Error> {
        user::update_user_partial(&mut **self, id, version, args).await
    }
    async fn soft_delete_user(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        user::soft_delete_user(&mut **self, id, version, when, updated_by).await
    }
    async fn touch_user_last_login_at(
        &mut self,
        id: i64,
        when: NaiveDateTime,
    ) -> Result<(), sqlx::Error> {
        user::touch_user_last_login_at(&mut **self, id, when).await
    }
    async fn increment_user_refresh_token_version(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        user::increment_user_refresh_token_version(&mut **self, id, version, when, updated_by).await
    }
    async fn update_user_password_and_rotate<'b>(
        &mut self,
        id: i64,
        version: i32,
        password_hash: &'b str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        user::update_user_password_and_rotate(
            &mut **self,
            id,
            version,
            password_hash,
            when,
            updated_by,
        )
        .await
    }

    // ── t_user_role（5）──
    async fn list_user_roles_by_user_id(
        &mut self,
        user_id: i64,
    ) -> Result<Vec<UserRoleRow>, sqlx::Error> {
        user_role::list_user_roles_by_user_id(&mut **self, user_id).await
    }
    async fn get_user_role_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<UserRole>, sqlx::Error> {
        user_role::get_user_role_by_id(&mut **self, id).await
    }
    async fn has_user_role_with_scope<'b>(
        &mut self,
        user_id: i64,
        role: &'b str,
        scope_type: Option<&'b str>,
        scope_id: Option<i64>,
    ) -> Result<bool, sqlx::Error> {
        user_role::has_user_role_with_scope(&mut **self, user_id, role, scope_type, scope_id).await
    }
    async fn create_user_role(&mut self, role_row: &UserRoleInsert) -> Result<(), sqlx::Error> {
        user_role::create_user_role(&mut **self, role_row).await
    }
    async fn soft_delete_user_role(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        user_role::soft_delete_user_role(&mut **self, id, version, when, updated_by).await
    }

    // ── t_menu（1）──
    async fn list_active_menus_by_roles<'b>(
        &mut self,
        roles: &'b [String],
    ) -> Result<Vec<Menu>, sqlx::Error> {
        menu::list_active_menus_by_roles(&mut **self, roles).await
    }

    // ── t_shelf（1）──
    async fn get_shelf_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<Shelf>, sqlx::Error> {
        shelf::get_shelf_by_id(&mut **self, id).await
    }
}